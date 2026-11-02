//! Parts that ask the user something: permission approvals and forms.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use blinc_cn::prelude::{input, ButtonBuilder, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_input::{text_input_state, SharedTextInputData};
use muqun_gateway::agents::{
    AgentSessionId, FormField, FormRequest, PermissionDecision, PermissionRequest,
};
use serde_json::Value;

use super::{card, meta, mono_block, scale};
use crate::model::commands::Command;
use crate::ui::text::ui_text;
use crate::ui::App;

const KEY_Y: u32 = 89;
const KEY_N: u32 = 78;

fn pending(app: &App, request_id: &str) -> bool {
    app.bindings
        .permissions
        .get()
        .iter()
        .any(|p| p.id == request_id)
}

/// The first permission still waiting in the shown session: the one Y / N
/// answer.
fn first_pending(app: &App) -> Option<String> {
    app.bindings.permissions.get().first().map(|p| p.id.clone())
}

fn reply(app: &App, asid: &AgentSessionId, id: &str, decision: PermissionDecision) {
    app.send(Command::ReplyPermission {
        asid: asid.clone(),
        id: id.to_string(),
        decision,
    });
}

/// A permission card. While it waits: Allow / Deny, and Y / N while the
/// composer does not have the keyboard and this is the oldest waiting
/// request. Once answered: a muted line.
pub fn approval(app: &App, request: &PermissionRequest, item: Option<&str>) -> Div {
    let strings = &app.strings;
    let waiting = pending(app, &request.id);
    let mut body = div()
        .class("mq-approval-body")
        .flex_col()
        .child(
            ui_text(if waiting {
                strings.permission_needed()
            } else {
                strings.resolved()
            })
            .size(scale::FS_12)
            .semibold(),
        )
        .child(
            ui_text(request.prompt.clone())
                .size(scale::BODY)
                .line_height(1.5),
        );
    if !request.resources.is_empty() {
        body = body.child(mono_block(request.resources.join("\n")));
    }
    if !waiting {
        return card("mq-accent--subtle", body);
    }
    let button = |label: &'static str, variant: ButtonVariant, decision: PermissionDecision| {
        let app = app.clone();
        let asid = request.asid.clone();
        let id = request.id.clone();
        let key = format!(
            "mq-perm.{}.{}.{}",
            id,
            item.unwrap_or("pending"),
            decision.as_str()
        );
        ButtonBuilder::with_key(key, label)
            .variant(variant)
            .size(ButtonSize::Small)
            .on_click(move |_| reply(&app, &asid, &id, decision))
    };
    body = body.child(
        div()
            .class("mq-approval-actions")
            .flex_row()
            .items_center()
            .child(button(
                strings.approve(),
                ButtonVariant::Primary,
                PermissionDecision::Allow,
            ))
            .child(button(
                strings.deny(),
                ButtonVariant::Outline,
                PermissionDecision::Deny,
            ))
            .child(meta("Y / N")),
    );
    let keys = app.clone();
    let asid = request.asid.clone();
    let id = request.id.clone();
    card("mq-accent--warning", body).on_key_down(move |ctx| {
        if ctx.ctrl || ctx.meta || ctx.alt {
            return;
        }
        let composing = keys.composer.lock().map(|s| s.is_focused()).unwrap_or(true);
        if composing || first_pending(&keys).as_deref() != Some(id.as_str()) {
            return;
        }
        match ctx.key_code {
            KEY_Y => reply(&keys, &asid, &id, PermissionDecision::Allow),
            KEY_N => reply(&keys, &asid, &id, PermissionDecision::Deny),
            _ => {}
        }
    })
}

/// A form's draft answers and text fields, by form id.
#[derive(Default)]
struct Draft {
    values: HashMap<String, Value>,
    inputs: HashMap<String, SharedTextInputData>,
}

fn drafts() -> &'static Mutex<HashMap<String, Draft>> {
    static DRAFTS: OnceLock<Mutex<HashMap<String, Draft>>> = OnceLock::new();
    DRAFTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn input_for(form: &str, key: &str, initial: Option<String>) -> SharedTextInputData {
    let mut d = drafts().lock().unwrap();
    let draft = d.entry(form.to_string()).or_default();
    draft
        .inputs
        .entry(key.to_string())
        .or_insert_with(|| {
            let data = text_input_state();
            if let (Some(v), Ok(mut s)) = (initial, data.lock()) {
                s.cursor = v.chars().count();
                s.value = v;
            }
            data
        })
        .clone()
}

fn value_of(form: &str, key: &str) -> Option<Value> {
    drafts()
        .lock()
        .unwrap()
        .get(form)
        .and_then(|d| d.values.get(key).cloned())
}

fn field_key(field: &FormField) -> (&str, &str, Option<&str>) {
    match field {
        FormField::String {
            key,
            title,
            description,
            ..
        }
        | FormField::Number {
            key,
            title,
            description,
            ..
        }
        | FormField::Boolean {
            key,
            title,
            description,
            ..
        }
        | FormField::Multiselect {
            key,
            title,
            description,
            ..
        }
        | FormField::External {
            key,
            title,
            description,
            ..
        } => (key, title, description.as_deref()),
        FormField::Unknown { key, title, .. } => (key, title, None),
    }
}

/// A row of choice chips. `multi`: toggles; else picks one.
fn chips(
    app: &App,
    form: &FormRequest,
    key: &str,
    options: Vec<(String, String)>,
    multi: bool,
) -> Div {
    let current = value_of(&form.id, key);
    let mut row = div().class("mq-chips").flex_row().flex_wrap();
    for (value, label) in options {
        let selected = match &current {
            Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(value.as_str())),
            Some(v) => v.as_str() == Some(value.as_str()) || *v == Value::String(value.clone()),
            None => false,
        };
        let form_id = form.id.clone();
        let key = key.to_string();
        let rows = app.bindings.rows.clone();
        let asid = form.asid.0.clone();
        row = row.child(
            div()
                .class("mq-chip")
                .class_when("mq-chip--active", selected)
                .cursor_pointer()
                .child(ui_text(label).size(scale::FS_12).no_wrap().no_cursor())
                .on_click(move |_| {
                    {
                        let mut d = drafts().lock().unwrap();
                        let draft = d.entry(form_id.clone()).or_default();
                        let parsed = match value.as_str() {
                            "true" => Value::Bool(true),
                            "false" => Value::Bool(false),
                            v => Value::String(v.to_string()),
                        };
                        if multi {
                            let entry = draft
                                .values
                                .entry(key.clone())
                                .or_insert_with(|| Value::Array(Vec::new()));
                            if let Value::Array(a) = entry {
                                if let Some(at) = a.iter().position(|x| x == &parsed) {
                                    a.remove(at);
                                } else {
                                    a.push(parsed);
                                }
                            }
                        } else {
                            draft.values.insert(key.clone(), parsed);
                        }
                    }
                    // Forms render as rows of their own or as pending cards
                    // at the end of the list: bump every row of the form.
                    let ids = [format!("form:{form_id}")];
                    for (state, version) in rows.bump(&asid, ids.iter()) {
                        state.signal().set(version);
                    }
                    if let Some(scheduler) = blinc_animation::try_get_scheduler() {
                        scheduler.request_redraw();
                    }
                }),
        );
    }
    row
}

/// Collect the answers: chips from the draft, text from the inputs.
fn answers(form: &FormRequest) -> Value {
    let d = drafts().lock().unwrap();
    let mut out = serde_json::Map::new();
    let draft = d.get(&form.id);
    for field in &form.fields {
        let (key, _, _) = field_key(field);
        if let Some(v) = draft.and_then(|d| d.values.get(key)) {
            out.insert(key.to_string(), v.clone());
            continue;
        }
        let Some(text) = draft
            .and_then(|d| d.inputs.get(key))
            .and_then(|i| i.lock().ok().map(|s| s.value.clone()))
        else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let value = match field {
            FormField::Number { .. } => text
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or(Value::String(text)),
            _ => Value::String(text),
        };
        out.insert(key.to_string(), value);
    }
    Value::Object(out)
}

/// A form: one control per field, then Submit. Once answered, a muted line.
pub fn form(app: &App, request: &FormRequest) -> Div {
    let strings = &app.strings;
    let waiting = app.bindings.forms.get().iter().any(|f| f.id == request.id);
    let mut body = div()
        .class("mq-approval-body")
        .flex_col()
        .child(
            ui_text(if waiting {
                strings.answer_needed()
            } else {
                strings.answered()
            })
            .size(scale::FS_12)
            .semibold(),
        )
        .child(
            ui_text(request.title.clone())
                .size(scale::BODY)
                .line_height(1.5),
        );
    if !waiting {
        return card("mq-accent--subtle", body);
    }
    for field in &request.fields {
        let (key, title, description) = field_key(field);
        let mut block = div()
            .class("mq-form-field")
            .flex_col()
            .child(ui_text(title.to_string()).size(scale::FS_13).semibold());
        if let Some(d) = description.filter(|d| !d.is_empty()) {
            block = block.child(meta(d.to_string()));
        }
        let control: Div = match field {
            FormField::String {
                options,
                default,
                placeholder,
                custom,
                ..
            } if !options.is_empty() => {
                let mut c = div().flex_col().child(chips(
                    app,
                    request,
                    key,
                    options
                        .iter()
                        .map(|o| (o.value.clone(), o.label.clone()))
                        .collect(),
                    false,
                ));
                if *custom {
                    c = c.child(
                        input(&input_for(&request.id, key, default.clone()))
                            .placeholder(placeholder.clone().unwrap_or_default())
                            .w_full(),
                    );
                }
                c
            }
            FormField::String {
                default,
                placeholder,
                ..
            } => div().child(
                input(&input_for(&request.id, key, default.clone()))
                    .placeholder(placeholder.clone().unwrap_or_default())
                    .w_full(),
            ),
            FormField::Number { default, .. } => div().child(
                input(&input_for(&request.id, key, default.map(|d| d.to_string()))).w_full(),
            ),
            FormField::Boolean { .. } => chips(
                app,
                request,
                key,
                vec![
                    ("true".into(), strings.yes().into()),
                    ("false".into(), strings.no().into()),
                ],
                false,
            ),
            FormField::Multiselect { options, .. } => chips(
                app,
                request,
                key,
                options
                    .iter()
                    .map(|o| (o.value.clone(), o.label.clone()))
                    .collect(),
                true,
            ),
            FormField::External { url, .. } => {
                div().child(link(strings.open_link(), url.clone()).font_size(scale::FS_13))
            }
            FormField::Unknown { .. } => div().child(meta(strings.unsupported_field())),
        };
        body = body.child(block.child(control));
    }
    let submit_app = app.clone();
    let form = request.clone();
    body = body.child(
        div().class("mq-approval-actions").flex_row().child(
            ButtonBuilder::with_key(format!("mq-form.{}.submit", request.id), strings.submit())
                .variant(ButtonVariant::Primary)
                .size(ButtonSize::Small)
                .on_click(move |_| {
                    submit_app.send(Command::ReplyForm {
                        asid: form.asid.clone(),
                        id: form.id.clone(),
                        answers: answers(&form),
                    });
                }),
        ),
    );
    card("mq-accent--warning", body)
}
