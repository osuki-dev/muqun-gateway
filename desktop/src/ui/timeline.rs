//! The session timeline: one block per [`TimelineItem`], by [`AgentPart`].

use std::path::{Path, PathBuf};

use muqun_gateway::agents::message_images::resolve_host_media;
use muqun_gateway::agents::ports::agent::prompt_display_body;
use muqun_gateway::agents::MessageImageAsset;
use muqun_gateway::agents::{
    AgentPart, AgentSessionId, FormField, FormRequest, PermissionDecision, PermissionRequest,
    TimelineItem, TimelineRole, ToolCall, ToolCallStatus,
};
use rune::{col, el, row, text, El, ImageCache, ScrollSpec, SurfaceFit, TextEdit};
use rune_kit::diff::{DiffState, DiffView};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, Collapsible, ControlSize, Kit, Markdown,
    Note, ScrollBar, Spinner, Switch, TextInput,
};
use serde_json::{json, Value};

use super::{muted, UiState};
use crate::model::commands::Command;

const OUTPUT_CHARS: usize = 600;
const SUMMARY_CHARS: usize = 240;
const TIMELINE_IMG_W: f32 = 320.0;
const TIMELINE_IMG_H: f32 = 240.0;

thread_local! {
    static EMPTY: &'static DiffState = Box::leak(Box::new(DiffState::default()));
}

/// A diff with no files, for views whose state went away between frames.
pub fn empty_diff() -> &'static DiffState {
    EMPTY.with(|d| *d)
}

pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// The unified patch of an edit tool's `metadata.files`, joined.
pub fn tool_patch(call: &ToolCall) -> String {
    let files = call
        .metadata
        .as_ref()
        .and_then(|m| m.get("files"))
        .and_then(Value::as_array);
    files
        .into_iter()
        .flatten()
        .filter_map(|f| {
            f.get("patch")
                .or_else(|| f.get("diff"))
                .and_then(Value::as_str)
        })
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_output(call: &ToolCall) -> String {
    if let Some(output) = &call.output {
        return match output {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
    }
    call.content
        .as_ref()
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn tool_input(call: &ToolCall) -> String {
    let pick = [
        "command",
        "cmd",
        "CommandLine",
        "file_path",
        "filePath",
        "TargetFile",
        "path",
        "pattern",
        "query",
        "url",
        "uri",
    ];
    let root = call
        .input
        .get("arguments")
        .and_then(Value::as_object)
        .or_else(|| call.input.as_object());
    if let Some(obj) = root {
        if let Some(v) = pick
            .iter()
            .find_map(|k| obj.get(*k).and_then(Value::as_str))
        {
            return clip(v, SUMMARY_CHARS);
        }
        // MCP / structured tools: show a compact JSON of the args we have.
        if !obj.is_empty() {
            return clip(&Value::Object(obj.clone()).to_string(), SUMMARY_CHARS);
        }
    }
    match &call.input {
        Value::Null => call
            .input_partial
            .clone()
            .map(|p| clip(&p, SUMMARY_CHARS))
            .unwrap_or_default(),
        other => clip(&other.to_string(), SUMMARY_CHARS),
    }
}

fn looks_like_note(body: &str) -> bool {
    ["```facts", "```steps", "```callout", ":::facts", ":::steps"]
        .iter()
        .any(|m| body.contains(m))
}

fn rich(k: &Kit, id: &str, body: &str) -> El {
    if looks_like_note(body) {
        Note::new(body).key(id).build(k)
    } else {
        Markdown::new(body).build(k).key(format!("md-{id}"))
    }
}

/// Drop Codex-style `[Image #N]` tokens once the real pictures are shown.
fn strip_image_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("[Image #") {
        out.push_str(&rest[..at]);
        let after = &rest[at + "[Image #".len()..];
        match after.find(']') {
            Some(end) => {
                rest = after[end + 1..].trim_start();
            }
            None => {
                out.push_str(&rest[at..at + 1]);
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn image_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff")
    )
}

fn session_directory(s: &UiState) -> Option<&str> {
    s.current().and_then(|i| i.directory.as_deref())
}

fn push_resolved(out: &mut Vec<PathBuf>, src: &str, directory: Option<&str>) {
    let Some(path) = resolve_host_media(src, directory) else {
        return;
    };
    if !image_ext(&path) && !path.is_file() {
        return;
    }
    if path.is_file() && !out.iter().any(|p| p == &path) {
        out.push(path);
    }
}

/// App `messageImageForSource`: exact `src` match, image mime, not unavailable.
fn message_image_for_source<'a>(
    assets: Option<&'a [MessageImageAsset]>,
    src: &str,
) -> Option<&'a MessageImageAsset> {
    assets?.iter().find(|asset| {
        asset.src == src
            && asset.unavailable_reason.is_none()
            && asset.mime.starts_with("image/")
            && asset.mime[6..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    })
}

fn looks_like_image_uri(uri: &str) -> bool {
    uri.starts_with("data:image/")
        || matches!(
            Path::new(uri.split('?').next().unwrap_or(uri))
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp")
        )
}

/// App `MessageAttachments`: walk `attachments`, resolve each through
/// `image_assets`, else treat a clear image URI as a local file.
fn item_image_paths(s: &UiState, item: &TimelineItem) -> Vec<PathBuf> {
    let directory = session_directory(s);
    let mut paths = Vec::new();
    let assets = item.image_assets.as_deref();
    if let Some(attachments) = &item.attachments {
        for src in attachments {
            if let Some(asset) = message_image_for_source(assets, src) {
                let before = paths.len();
                for candidate in [
                    asset.thumbnail_url.as_deref(),
                    asset.preview_url.as_deref(),
                    (!asset.url.is_empty()).then_some(asset.url.as_str()),
                ]
                .into_iter()
                .flatten()
                {
                    push_resolved(&mut paths, candidate, directory);
                    if paths.len() > before {
                        break;
                    }
                }
                if paths.len() >= 8 {
                    return paths;
                }
                continue;
            }
            if looks_like_image_uri(src) {
                push_resolved(&mut paths, src, directory);
                if paths.len() >= 8 {
                    return paths;
                }
            }
        }
    }
    // Assistant markdown embeds: App rewrites sources via `image_assets` with
    // no `attachments` list; Desktop draws those assets as a gallery.
    if paths.is_empty() {
        if let Some(assets) = assets {
            for asset in assets {
                if asset.unavailable_reason.is_some() || !asset.mime.starts_with("image/") {
                    continue;
                }
                let before = paths.len();
                for candidate in [
                    asset.thumbnail_url.as_deref(),
                    asset.preview_url.as_deref(),
                    (!asset.url.is_empty()).then_some(asset.url.as_str()),
                    (!asset.src.is_empty()).then_some(asset.src.as_str()),
                ]
                .into_iter()
                .flatten()
                {
                    push_resolved(&mut paths, candidate, directory);
                    if paths.len() > before {
                        break;
                    }
                }
                if paths.len() >= 8 {
                    return paths;
                }
            }
        }
    }
    paths
}

fn tool_image_paths(s: &UiState, call: &ToolCall) -> Vec<PathBuf> {
    let directory = session_directory(s);
    let mut paths = Vec::new();
    let Some(Value::Array(content)) = &call.content else {
        return paths;
    };
    for part in content {
        // ACP nests media under `content`; Codex/OpenCode use top-level uri.
        let media = part
            .get("content")
            .filter(|inner| inner.is_object())
            .unwrap_or(part);
        let uri = media
            .get("uri")
            .and_then(Value::as_str)
            .or_else(|| media.get("url").and_then(Value::as_str))
            .or_else(|| media.get("path").and_then(Value::as_str));
        let Some(uri) = uri else {
            continue;
        };
        let is_image = media
            .get("mime")
            .or_else(|| media.get("mimeType"))
            .or_else(|| media.get("mediaType"))
            .and_then(Value::as_str)
            .is_some_and(|m| m.starts_with("image/"))
            || media.get("type").and_then(Value::as_str) == Some("image")
            || part.get("type").and_then(Value::as_str) == Some("image")
            || image_ext(Path::new(uri));
        if is_image || media.get("type").and_then(Value::as_str) == Some("file") {
            push_resolved(&mut paths, uri, directory);
        }
        if paths.len() >= 8 {
            break;
        }
    }
    paths
}

fn timeline_picture(k: &Kit, images: &ImageCache, path: &Path, key: &str) -> El {
    if !path.is_file() {
        return el()
            .key(key.to_string())
            .w(TIMELINE_IMG_W)
            .h(96.0)
            .radius(k.radius.md)
            .fill(k.c.layer_02);
    }
    // `ImageCache::file` keeps a strong handle via Pixels::keep while drawn.
    let image = images.file(path, TIMELINE_IMG_H, 2.0);
    el().key(key.to_string())
        .pixels_fit(image.pixels(), SurfaceFit::Fit)
        .w(TIMELINE_IMG_W)
        .h(TIMELINE_IMG_H)
        .radius(k.radius.md)
        .clip_rounded(k.radius.md)
}

fn image_gallery(s: &UiState, k: &Kit, id: &str, paths: &[PathBuf]) -> Option<El> {
    let images = s.images.as_ref()?;
    if paths.is_empty() {
        return None;
    }
    let mut gallery = row()
        .key(format!("imgs-{id}"))
        .gap(8.0)
        .flex_wrap()
        .w_full();
    for (i, path) in paths.iter().enumerate() {
        gallery = gallery.child(timeline_picture(k, images, path, &format!("img-{id}-{i}")));
    }
    Some(gallery)
}

fn mono(k: &Kit, body: String) -> El {
    text(body)
        .font(k.ty.mono)
        .color(k.c.text_muted)
        .wrap()
        .w_full()
}

fn badge(k: &Kit, label: impl Into<String>) -> El {
    Badge::new(label).variant(BadgeVariant::Subtle).build(k)
}

fn toggle(s: &mut UiState, id: &str) {
    if !s.open_items.remove(id) {
        s.open_items.insert(id.to_string());
    }
}

fn collapsible(s: &UiState, k: &Kit, id: &str, title: String, body: El) -> El {
    let open = s.open_items.contains(id);
    let key = id.to_string();
    Collapsible::new(title, open)
        .child(body)
        .on_toggle(move |s: &mut UiState| toggle(s, &key))
        .build(k)
        .key(format!("fold-{id}"))
}

fn inline_diff(s: &UiState, k: &Kit, id: &str) -> Option<El> {
    let (patch, _) = s.inline_diffs.get(id)?;
    let height = (patch.lines().count() as f32 * 20.0 + 44.0).clamp(88.0, 360.0);
    let (get_id, set_id) = (id.to_string(), id.to_string());
    let view = DiffView::new(
        move |s: &UiState| match s.inline_diffs.get(&get_id) {
            Some((_, d)) => d,
            None => empty_diff(),
        },
        move |s: &mut UiState| {
            &mut s
                .inline_diffs
                .entry(set_id.clone())
                .or_insert_with(|| (String::new(), DiffState::default()))
                .1
        },
    )
    .key(format!("diff-{id}"))
    .build(s, k);
    Some(
        col()
            .key(format!("diffbox-{id}"))
            .w_full()
            .h(height)
            .radius(k.radius.md)
            .clip_rounded(k.radius.md)
            .child(view),
    )
}

fn status_label(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::Streaming => "streaming",
        ToolCallStatus::Running => "running",
        ToolCallStatus::Completed => "done",
        ToolCallStatus::Failed => "failed",
    }
}

fn tool(s: &UiState, k: &Kit, item: &TimelineItem, call: &ToolCall) -> El {
    let accent = match call.state {
        ToolCallStatus::Completed => k.c.success.icon,
        ToolCallStatus::Failed => k.c.danger.icon,
        ToolCallStatus::Running | ToolCallStatus::Streaming => k.c.info.icon,
        ToolCallStatus::Pending => k.c.icon_faint,
    };
    let title = call
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| call.name.clone());
    let running = matches!(
        call.state,
        ToolCallStatus::Running | ToolCallStatus::Streaming | ToolCallStatus::Pending
    );
    let mut card = Card::new()
        .accent(accent)
        .title(format!("{title} · {}", status_label(call.state)), None);
    if running {
        card = card.child(
            row()
                .key(format!("run-{}", item.id))
                .gap(8.0)
                .center_y()
                .child(Spinner::new().build(k).key(format!("spin-{}", item.id)))
                .child(muted(k, "Working…")),
        );
    }
    let input = tool_input(call);
    if !input.is_empty() {
        card = card.child(mono(k, input));
    }
    if let Some(gallery) = image_gallery(
        s,
        k,
        &format!("tool-{}", item.id),
        &tool_image_paths(s, call),
    ) {
        card = card.child(gallery);
    }
    if let Some(inline) = inline_diff(s, k, &item.id) {
        card = card.child(inline);
    } else {
        let output = tool_output(call);
        if !output.is_empty() {
            let clipped = clip(&output, OUTPUT_CHARS);
            // Short results stay visible; long logs fold so the feed stays scannable.
            if clipped.chars().count() <= SUMMARY_CHARS && !call.truncated {
                card = card.child(mono(k, clipped));
            } else {
                let id = format!("out-{}", item.id);
                let label = if call.truncated {
                    "Output (truncated)"
                } else {
                    "Output"
                };
                card = card.child(collapsible(s, k, &id, label.into(), mono(k, clipped)));
            }
        }
    }
    if let Some(err) = &call.error {
        card = card.description(err.message.clone());
    }
    if let Some(child) = &call.child_session_id {
        let id = child.clone();
        card = card.action(
            Button::new("Open sub-agent")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.open_session(&id))
                .build(k)
                .key(format!("sub-{}", item.id)),
        );
    }
    card.build(k)
}

fn reply(
    request: &PermissionRequest,
    decision: PermissionDecision,
) -> impl Fn(&mut UiState) + 'static {
    let (asid, id) = (request.asid.clone(), request.id.clone());
    move |s: &mut UiState| {
        if let Some(list) = s.waiting_perms.get_mut(&asid.0) {
            list.retain(|p| p.id != id);
        }
        s.send(Command::ReplyPermission {
            asid: asid.clone(),
            id: id.clone(),
            decision,
        });
    }
}

pub fn pending_permission(s: &UiState, request: &PermissionRequest) -> bool {
    s.waiting_perms
        .get(&request.asid.0)
        .is_some_and(|l| l.iter().any(|p| p.id == request.id))
}

/// A permission as a card with its answers (or "resolved" once answered).
pub fn permission_card(s: &UiState, k: &Kit, request: &PermissionRequest) -> El {
    let mut card = Card::new()
        .accent(k.c.warning.icon)
        .title(format!("Permission: {}", request.action), None)
        .description(if request.prompt.is_empty() {
            request.action.clone()
        } else {
            request.prompt.clone()
        });
    if !request.resources.is_empty() {
        card = card.child(mono(k, clip(&request.resources.join("\n"), SUMMARY_CHARS)));
    }
    let key = |n: &str| format!("perm-{}-{n}", request.id);
    if pending_permission(s, request) {
        let button = |label: &str, variant, decision| {
            Button::new(label)
                .variant(variant)
                .control_size(ControlSize::Small)
                .on_press(reply(request, decision))
                .build(k)
                .key(key(label))
        };
        card = card
            .action(button(
                "Allow",
                ButtonVariant::Submit,
                PermissionDecision::Allow,
            ))
            .action(button(
                "Deny",
                ButtonVariant::Danger,
                PermissionDecision::Deny,
            ));
        if !request.save.is_empty()
            || request
                .options
                .iter()
                .any(|o| o.decision == PermissionDecision::AllowAlways)
        {
            card = card.action(button(
                "Always",
                ButtonVariant::Outline,
                PermissionDecision::AllowAlways,
            ));
        }
    } else {
        card = card.child(badge(k, "resolved"));
    }
    card.build(k)
}

fn form_key(form: &str, field: &str) -> String {
    format!("{form}/{field}")
}

fn form_value(s: &UiState, form: &str, field: &FormField) -> String {
    let (key, default) = field_meta(field);
    s.form_inputs
        .get(&form_key(form, key))
        .map_or(default, |e| e.text.clone())
}

/// A field's key and its default as text.
fn field_meta(field: &FormField) -> (&str, String) {
    match field {
        FormField::String { key, default, .. } => (key, default.clone().unwrap_or_default()),
        FormField::Number { key, default, .. } => {
            (key, default.map(|d| d.to_string()).unwrap_or_default())
        }
        FormField::Boolean { key, default, .. } => (key, default.unwrap_or(false).to_string()),
        FormField::Multiselect { key, default, .. } => (key, default.join(",")),
        FormField::External { key, .. } | FormField::Unknown { key, .. } => (key, String::new()),
    }
}

fn field_when(field: &FormField) -> &[muqun_gateway::agents::FormWhen] {
    match field {
        FormField::String { when, .. }
        | FormField::Number { when, .. }
        | FormField::Boolean { when, .. }
        | FormField::Multiselect { when, .. }
        | FormField::External { when, .. }
        | FormField::Unknown { when, .. } => when,
    }
}

fn visible(s: &UiState, form: &FormRequest, field: &FormField) -> bool {
    field_when(field).iter().all(|w| {
        let current = form
            .fields
            .iter()
            .find(|f| field_meta(f).0 == w.key)
            .map(|f| form_value(s, &form.id, f))
            .unwrap_or_default();
        let wanted = match &w.value {
            Value::String(v) => v.clone(),
            other => other.to_string(),
        };
        (current == wanted) == (w.op != "neq")
    })
}

fn form_answers(s: &UiState, form: &FormRequest) -> Value {
    let mut answers = serde_json::Map::new();
    for field in form.fields.iter().filter(|f| visible(s, form, f)) {
        let value = form_value(s, &form.id, field);
        let (key, _) = field_meta(field);
        let json = match field {
            FormField::Number { .. } => value
                .trim()
                .parse::<f64>()
                .map_or(Value::Null, |n| json!(n)),
            FormField::Boolean { .. } => json!(value == "true"),
            FormField::Multiselect { .. } => json!(value
                .split(',')
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()),
            FormField::External { .. } | FormField::Unknown { .. } => continue,
            FormField::String { .. } => json!(value),
        };
        answers.insert(key.to_string(), json);
    }
    Value::Object(answers)
}

fn pick(form: &str, key: &str, value: String, multi: bool) -> impl Fn(&mut UiState) + 'static {
    let slot = form_key(form, key);
    move |s: &mut UiState| {
        let next = if multi {
            let mut chosen: Vec<String> = s
                .form_inputs
                .get(&slot)
                .map(|e| {
                    e.text
                        .split(',')
                        .filter(|v| !v.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            match chosen.iter().position(|c| *c == value) {
                Some(at) => {
                    chosen.remove(at);
                }
                None => chosen.push(value.clone()),
            }
            chosen.join(",")
        } else {
            value.clone()
        };
        s.form_inputs.insert(slot.clone(), TextEdit::new(next));
    }
}

fn field_input(s: &UiState, k: &Kit, form: &FormRequest, field: &FormField) -> El {
    let (key, _) = field_meta(field);
    let slot = form_key(&form.id, key);
    let current = form_value(s, &form.id, field);
    let (title, description, options, multi): (
        &str,
        Option<&String>,
        &[muqun_gateway::agents::FormOption],
        bool,
    ) = match field {
        FormField::String {
            title,
            description,
            options,
            ..
        } => (title, description.as_ref(), options, false),
        FormField::Number {
            title, description, ..
        }
        | FormField::Boolean {
            title, description, ..
        } => (title, description.as_ref(), &[], false),
        FormField::Multiselect {
            title,
            description,
            options,
            ..
        } => (title, description.as_ref(), options, true),
        FormField::External {
            title, description, ..
        } => (title, description.as_ref(), &[], false),
        FormField::Unknown { title, .. } => (title, None, &[], false),
    };
    let control = match field {
        FormField::Boolean { .. } => Switch::new(current == "true")
            .label(title)
            .on_toggle({
                let slot = slot.clone();
                move |s: &mut UiState| {
                    let on = s.form_inputs.get(&slot).is_some_and(|e| e.text == "true");
                    s.form_inputs
                        .insert(slot.clone(), TextEdit::new((!on).to_string()));
                }
            })
            .build(k),
        FormField::External { url, .. } => muted(k, url.clone()),
        FormField::Unknown { raw_type, .. } => {
            muted(k, format!("Unsupported field type: {raw_type}"))
        }
        _ if !options.is_empty() => {
            let chosen: Vec<&str> = current.split(',').collect();
            let mut line = row().key(format!("opts-{slot}")).gap(6.0).wrap();
            for option in options {
                let on = chosen.contains(&option.value.as_str());
                line = line.child(
                    Button::new(option.label.clone())
                        .variant(if on {
                            ButtonVariant::Contrast
                        } else {
                            ButtonVariant::Outline
                        })
                        .control_size(ControlSize::Small)
                        .on_press(pick(&form.id, key, option.value.clone(), multi))
                        .build(k)
                        .key(format!("opt-{slot}-{}", option.value)),
                );
            }
            line
        }
        _ => {
            let edit = s
                .form_inputs
                .get(&slot)
                .cloned()
                .unwrap_or_else(|| TextEdit::new(current.clone()));
            let target = slot.clone();
            TextInput::new(&edit)
                .placeholder(title)
                .label(title)
                .on_edit(move |s: &mut UiState| {
                    s.form_inputs
                        .entry(target.clone())
                        .or_insert_with(|| TextEdit::new(current.clone()))
                })
                .build(k)
                .key(format!("in-{slot}"))
        }
    };
    let mut block = col().key(format!("field-{slot}")).w_full().gap(4.0);
    if !matches!(field, FormField::Boolean { .. }) {
        block = block.child(
            text(title.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_base),
        );
    }
    if let Some(d) = description {
        block = block.child(muted(k, d.clone()));
    }
    block.child(control)
}

pub fn form_card(s: &UiState, k: &Kit, form: &FormRequest) -> El {
    let pending = s
        .waiting_forms
        .get(&form.asid.0)
        .is_some_and(|l| l.iter().any(|f| f.id == form.id));
    let mut card = Card::new()
        .accent(k.c.info.icon)
        .title(form.title.clone(), None);
    if !pending {
        return card.child(badge(k, "answered")).build(k);
    }
    for field in form.fields.iter().filter(|f| visible(s, form, f)) {
        card = card.child(field_input(s, k, form, field));
    }
    let (asid, id) = (form.asid.clone(), form.id.clone());
    card.action(
        Button::new("Submit")
            .variant(ButtonVariant::Submit)
            .control_size(ControlSize::Small)
            .on_press({
                let form = form.clone();
                move |s: &mut UiState| {
                    let answers = form_answers(s, &form);
                    if let Some(list) = s.waiting_forms.get_mut(&asid.0) {
                        list.retain(|f| f.id != id);
                    }
                    s.send(Command::ReplyForm {
                        asid: asid.clone(),
                        id: id.clone(),
                        answers,
                    });
                }
            })
            .build(k)
            .key(format!("form-submit-{}", form.id)),
    )
    .build(k)
}

fn todo(k: &Kit, items: &[muqun_gateway::agents::TodoItem]) -> El {
    let mut list = col().gap(4.0).w_full();
    for (i, item) in items.iter().enumerate() {
        let mark = if item.done { "✓" } else { "○" };
        list = list.child(
            row()
                .key(format!("todo-{i}"))
                .gap(8.0)
                .child(text(mark).font(k.ty.ui).color(if item.done {
                    k.c.success.icon
                } else {
                    k.c.text_faint
                }))
                .child(
                    text(item.text.clone())
                        .font(k.ty.ui)
                        .color(k.c.text_base)
                        .wrap()
                        .grow(1.0),
                ),
        );
    }
    Card::new()
        .accent(k.c.icon_accent)
        .title("Todo", None)
        .child(list)
        .build(k)
}

fn shell_card(s: &UiState, k: &Kit, item: &TimelineItem, part: &AgentPart) -> El {
    let AgentPart::Shell {
        command,
        status,
        exit,
        output,
        truncated,
        ..
    } = part
    else {
        return col();
    };
    let mut card = Card::new()
        .accent(if exit.is_some_and(|e| e != 0.0) {
            k.c.danger.icon
        } else {
            k.c.icon_base
        })
        .title(format!("$ {}", clip(command, 120)), None)
        .description(match exit {
            Some(code) => format!("{status} · exit {code}"),
            None => status.clone(),
        });
    if let Some(out) = output.as_ref().filter(|o| !o.is_empty()) {
        let label = if *truncated {
            "Output (truncated)"
        } else {
            "Output"
        };
        card = card.child(collapsible(
            s,
            k,
            &format!("sh-{}", item.id),
            label.into(),
            mono(k, clip(out, OUTPUT_CHARS)),
        ));
    }
    card.build(k)
}

fn part(s: &UiState, k: &Kit, item: &TimelineItem) -> El {
    let id = item.id.as_str();
    match &item.part {
        AgentPart::Text { text } => {
            let paths = item_image_paths(s, item);
            // App shows prose first, then MessageAttachments underneath.
            let body = {
                let prose = prompt_display_body(text);
                if paths.is_empty() {
                    text.clone()
                } else {
                    strip_image_placeholders(prose)
                }
            };
            let mut block = col().key(format!("text-{id}")).gap(8.0).w_full();
            if !body.is_empty() {
                block = block.child(rich(k, id, &body));
            }
            if let Some(gallery) = image_gallery(s, k, id, &paths) {
                block = block.child(gallery);
            }
            block
        }
        AgentPart::Reasoning { text, duration_ms } => {
            let title = match duration_ms {
                Some(ms) => format!("Thinking · {:.1}s", *ms as f32 / 1000.0),
                None => "Thinking".into(),
            };
            collapsible(s, k, id, title, rich(k, id, text))
        }
        AgentPart::Tool(call) => tool(s, k, item, call),
        AgentPart::Diff { file, .. } => {
            let mut card = Card::new()
                .accent(k.c.icon_accent)
                .title(file.clone(), None);
            if let Some(diff) = inline_diff(s, k, id) {
                card = card.child(diff);
            }
            card.build(k)
        }
        AgentPart::Todo { items } => todo(k, items),
        AgentPart::Approval { request } => permission_card(s, k, request),
        AgentPart::Form { request } => form_card(s, k, request),
        AgentPart::Status { text } => badge(k, text.clone()),
        AgentPart::Compaction {
            status,
            reason,
            summary,
            ..
        } => {
            let mut card = Card::new()
                .accent(k.c.icon_muted)
                .title(format!("Compaction · {status:?}").to_lowercase(), None);
            if let Some(reason) = reason {
                card = card.description(reason.clone());
            }
            if let Some(summary) = summary.as_ref().filter(|t| !t.is_empty()) {
                card = card.child(collapsible(
                    s,
                    k,
                    &format!("cmp-{id}"),
                    "Summary".into(),
                    rich(k, id, summary),
                ));
            }
            card.build(k)
        }
        AgentPart::Skill { skill, name, text } => col()
            .gap(6.0)
            .child(badge(
                k,
                format!("Skill · {}", if name.is_empty() { skill } else { name }),
            ))
            .child(rich(k, id, text)),
        AgentPart::Shell { .. } => shell_card(s, k, item, &item.part),
        AgentPart::ModelSwitched { model, .. } => badge(
            k,
            format!("Model → {}/{}", model.provider_id, model.model_id),
        ),
        AgentPart::AgentSwitched { mode, .. } => badge(k, format!("Mode → {mode}")),
        AgentPart::LocationSwitched { directory, .. } => badge(k, format!("Moved to {directory}")),
        AgentPart::Synthetic { text, description } | AgentPart::System { text, description } => {
            let mut block = col().gap(6.0);
            if let Some(d) = description.as_ref().filter(|d| !d.is_empty()) {
                block = block.child(badge(k, d.clone()));
            }
            block.child(rich(k, id, text))
        }
    }
}

/// Rollback and Fork from here, under a message's text.
fn message_actions(k: &Kit, it: &TimelineItem, asid: Option<&str>) -> Option<El> {
    let asid = AgentSessionId(asid?.to_string());
    let (rollback_asid, rollback_id) = (asid.clone(), it.message_id.clone());
    let fork_id = it.message_id.clone();
    Some(
        row()
            .key(format!("actions-{}", it.id))
            .gap(4.0)
            .center_y()
            .child(
                Button::new("Rollback")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.send(Command::StageRevert {
                            asid: rollback_asid.clone(),
                            message_id: rollback_id.clone(),
                        })
                    })
                    .build(k)
                    .key(format!("rollback-{}", it.id)),
            )
            .child(
                Button::new("Fork from here")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.send(Command::ForkSession {
                            asid: asid.clone(),
                            message_id: Some(fork_id.clone()),
                        })
                    })
                    .build(k)
                    .key(format!("fork-{}", it.id)),
            ),
    )
}

fn item(s: &UiState, k: &Kit, it: &TimelineItem, asid: Option<&str>) -> El {
    let body = part(s, k, it);
    let mut wrap = col().key(format!("item-{}", it.id)).w_full().gap(2.0);
    let is_text = matches!(it.part, AgentPart::Text { .. });
    let user_text = it.role == TimelineRole::User && is_text;
    wrap = if user_text {
        wrap.child(
            col()
                .key("user")
                .w_full()
                .px(12.0)
                .py(8.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_02)
                .child(body),
        )
    } else {
        wrap.child(body)
    };
    if is_text && matches!(it.role, TimelineRole::User | TimelineRole::Assistant) {
        if let Some(actions) = message_actions(k, it, asid) {
            wrap = wrap.child(actions);
        }
    }
    wrap
}

pub fn view(s: &UiState, k: &Kit) -> El {
    view_items(
        s,
        k,
        s.selected.as_deref(),
        &s.timeline,
        s.earlier_exhausted,
        s.busy(),
    )
}

/// The feed of `items` for session `asid` (a split tile's own timeline; the
/// focused session passes the top-level fields through [`view`]).
pub fn view_items(
    s: &UiState,
    k: &Kit,
    asid: Option<&str>,
    items: &[TimelineItem],
    earlier_exhausted: bool,
    busy: bool,
) -> El {
    let mut feed = col().key("feed").gap(12.0).w_full().px(24.0).py(24.0);
    if items.is_empty() {
        let line = if asid.is_some() {
            "No messages yet."
        } else {
            "Select a session or start a new one."
        };
        feed = feed.child(
            text(line)
                .key("empty-feed")
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    } else if !earlier_exhausted {
        if let Some(asid) = asid.map(str::to_string) {
            feed = feed.child(
                Button::new("Load earlier")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.send(Command::LoadEarlier(muqun_gateway::agents::AgentSessionId(
                            asid.clone(),
                        )));
                    })
                    .build(k)
                    .key("load-earlier"),
            );
        }
    }
    for it in items {
        feed = feed.child(item(s, k, it, asid));
    }
    if busy {
        feed = feed.child(
            row()
                .key("working")
                .gap(8.0)
                .center_y()
                .child(Spinner::new().build(k))
                .child(text("Working…").font(k.ty.small).color(k.c.text_muted)),
        );
    }
    feed.scroll(ScrollSpec { stick_bottom: true })
        .grow(1.0)
        .min_h(0.0)
        .scroll_bar(k)
}

/// The session's working-tree changes, full height.
pub fn changes(s: &UiState, k: &Kit) -> El {
    if s.diff.is_none() {
        let mut list = col()
            .key("changes-list")
            .gap(6.0)
            .w_full()
            .px(24.0)
            .py(24.0);
        list = list.child(muted(
            k,
            format!(
                "{} changed file(s); the agent gave no patch.",
                s.diff_stat.files
            ),
        ));
        for f in &s.diff_stat.paths {
            let (asid, path) = (s.selected.clone(), f.path.clone());
            list = list.child(
                row()
                    .key(format!("chg-{}", f.path))
                    .gap(8.0)
                    .center_y()
                    .child(
                        Button::new(f.path.clone())
                            .variant(ButtonVariant::Ghost)
                            .control_size(ControlSize::Small)
                            .on_press(move |s: &mut UiState| {
                                if let Some(asid) = asid.clone() {
                                    s.send(Command::OpenDiff {
                                        asid: AgentSessionId(asid),
                                        file: Some(path.clone()),
                                    });
                                }
                            })
                            .build(k)
                            .key(format!("chg-open-{}", f.path))
                            .grow(1.0),
                    )
                    .child(badge(k, format!("+{} −{}", f.added, f.removed))),
            );
        }
        return list
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .grow(1.0)
            .min_h(0.0)
            .scroll_bar(k);
    }
    col().key("changes").w_full().grow(1.0).min_h(0.0).child(
        DiffView::new(
            |s: &UiState| s.diff.as_ref().unwrap_or_else(|| empty_diff()),
            |s: &mut UiState| s.diff.get_or_insert_with(DiffState::default),
        )
        .key("changes-diff")
        .build(s, k),
    )
}
