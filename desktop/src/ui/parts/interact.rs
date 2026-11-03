//! Parts that ask the user something: permission approvals and forms.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use blinc_cn::prelude::{input, kbd, switch, KbdSize, SwitchSize};
use blinc_core::State;
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_input::{text_input_state, SharedTextInputData};
use blinc_theme::{ColorToken, ThemeState};
use muqun_gateway::agents::{FormField, FormRequest, PermissionDecision, PermissionRequest};
use serde_json::Value;

use super::{card, meta, mono, scale};
use crate::model::commands::Command;
use crate::ui::text::ui_text;
use crate::ui::App;

pub const KEY_Y: u32 = 89;
pub const KEY_N: u32 = 78;

fn pending(app: &App, request_id: &str) -> bool {
    app.bindings
        .permissions
        .get()
        .iter()
        .any(|p| p.id == request_id)
}

/// Whether the agent can remember an answer for the rest of the session
/// ("always" replies): OpenCode names what it would save, ACP agents offer
/// an allow-always option.
fn supports_remember(request: &PermissionRequest) -> bool {
    !request.save.is_empty()
        || request
            .options
            .iter()
            .any(|o| o.decision == PermissionDecision::AllowAlways)
}

/// The "remember for this session" switch of a request.
fn remembered(id: &str) -> State<bool> {
    blinc_core::BlincContextState::get()
        .use_state_keyed(&format!("mq.perm.remember.{id}"), || false)
}

/// Answer `request`: Allow becomes "always" when the switch is on.
fn decide(app: &App, request: &PermissionRequest, allow: bool) {
    let decision = match (
        allow,
        supports_remember(request) && remembered(&request.id).get(),
    ) {
        (false, _) => PermissionDecision::Deny,
        (true, true) => PermissionDecision::AllowAlways,
        (true, false) => PermissionDecision::Allow,
    };
    app.send(Command::ReplyPermission {
        asid: request.asid.clone(),
        id: request.id.clone(),
        decision,
    });
}

/// Y / N: answer the oldest waiting approval of the shown session. For the
/// timeline's key handler, which runs whatever has the keyboard: it keeps out
/// of the way of the composer, a form's fields and the pages that cover the
/// timeline. True when the key was an answer.
pub fn answer_key(app: &App, key_code: u32) -> bool {
    if !matches!(key_code, KEY_Y | KEY_N)
        || app.composer.lock().map(|s| s.is_focused()).unwrap_or(true)
        || app.bindings.settings_open.get()
        || app.bindings.draft.get().is_some()
        || !app.bindings.forms.get().is_empty()
    {
        return false;
    }
    let Some(request) = app.bindings.permissions.get().first().cloned() else {
        return false;
    };
    decide(app, &request, key_code == KEY_Y);
    true
}

/// "Claude Code wants to run", from the session's agent.
fn lead(app: &App, action: &str) -> String {
    let agent = match app.bindings.header.get() {
        Some(info) => crate::ui::agents::name(&info.agent_id, &app.bindings.agents.get()),
        None => app.strings.unknown_agent().to_string(),
    };
    app.strings.wants_to(&agent, action)
}

/// What the request is about, on one line: the command or path (several
/// patterns read as one line, joined).
fn subject(request: &PermissionRequest) -> String {
    let one = |s: &str| s.lines().next().unwrap_or("").trim().to_string();
    let joined = request
        .resources
        .iter()
        .map(|r| one(r))
        .collect::<Vec<_>>()
        .join(" · ");
    if !joined.is_empty() {
        return joined;
    }
    request
        .metadata
        .as_ref()
        .and_then(|m| m.get("command"))
        .and_then(Value::as_str)
        .map(one)
        .unwrap_or_else(|| one(&request.prompt))
}

/// A button with its key cap: ghost for Deny, mint (and wider) for Allow.
fn choice(label: &str, key: &str, class: &str, id: String, on_click: impl Fn() + 'static) -> Div {
    div()
        .id(id)
        .class("mq-choice")
        .class(class)
        .flex_row()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(
            ui_text(label.to_string())
                .size(scale::FS_13)
                .semibold()
                .no_cursor()
                .class(if class.ends_with("allow") {
                    "mq-on-primary"
                } else {
                    "mq-text"
                }),
        )
        .child(kbd(key).size(KbdSize::Small))
        .on_click(move |_| on_click())
}

/// A permission card: an amber bar, one line (who, what, the command), and
/// at the right Deny `N` and Allow `Y`; under it, when the agent can, the
/// switch that makes Allow last for the session. Y / N answer from anywhere
/// in the timeline (`answer_key`). Once answered: a muted line.
pub fn approval(app: &App, request: &PermissionRequest, item: Option<&str>) -> Div {
    let strings = &app.strings;
    let waiting = pending(app, &request.id);
    let line = div()
        .class("mq-approval-line")
        .flex_1()
        .min_w(0.0)
        .flex_row()
        .items_center()
        .child(
            ui_text(lead(app, &request.action))
                .size(scale::BODY)
                .no_wrap()
                .class(if waiting { "mq-text" } else { "mq-text-muted" }),
        )
        .child(
            div()
                .class("mq-approval-subject")
                .flex_1()
                .min_w(0.0)
                .overflow_clip()
                .child(mono(subject(request)).class("mq-text")),
        );
    if !waiting {
        return card(
            "mq-accent--subtle",
            div()
                .flex_row()
                .items_center()
                .child(line)
                .child(meta(strings.resolved()).class("mq-text-subtle")),
        );
    }
    let key = |what: &str| {
        format!(
            "mq-perm.{}.{}.{what}",
            request.id,
            item.unwrap_or("pending")
        )
    };
    let (allow_app, allow_req) = (app.clone(), request.clone());
    let (deny_app, deny_req) = (app.clone(), request.clone());
    let mut top = div()
        .class("mq-approval-top")
        .w_full()
        .flex_row()
        .items_center()
        .child(line)
        .child(choice(
            strings.deny(),
            "N",
            "mq-choice--deny",
            key("deny"),
            move || decide(&deny_app, &deny_req, false),
        ))
        .child(choice(
            strings.allow(),
            "Y",
            "mq-choice--allow",
            key("allow"),
            move || decide(&allow_app, &allow_req, true),
        ));
    top = top.class("mq-approval-top");
    let mut body = div().class("mq-approval-body").flex_col().child(top);
    if supports_remember(request) {
        let state = remembered(&request.id);
        body = body.child(
            div()
                .class("mq-approval-remember")
                .flex_row()
                .items_center()
                .child(
                    switch(&state)
                        .size(SwitchSize::Small)
                        .off_color(ThemeState::get().color(ColorToken::BorderSecondary)),
                )
                .child(meta(strings.remember_session())),
        );
    }
    card("mq-accent--warning", body)
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
    let line = div()
        .class("mq-approval-line")
        .flex_1()
        .min_w(0.0)
        .flex_row()
        .items_center()
        .child(
            ui_text(if waiting {
                strings.answer_needed()
            } else {
                strings.answered()
            })
            .size(scale::BODY)
            .no_wrap()
            .class(if waiting { "mq-text" } else { "mq-text-muted" }),
        )
        .child(
            div()
                .class("mq-approval-subject")
                .flex_1()
                .min_w(0.0)
                .overflow_clip()
                .child(
                    ui_text(request.title.clone())
                        .size(scale::BODY)
                        .no_wrap()
                        .class("mq-text-muted"),
                ),
        );
    if !waiting {
        return card("mq-accent--subtle", div().flex_row().child(line));
    }
    let submit_app = app.clone();
    let submitted = request.clone();
    let top = div()
        .class("mq-approval-top")
        .w_full()
        .flex_row()
        .items_center()
        .child(line)
        .child(
            div()
                .id(format!("mq-form.{}.submit", request.id))
                .class("mq-choice")
                .class("mq-choice--allow")
                .flex_row()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(
                    ui_text(strings.submit())
                        .size(scale::FS_13)
                        .semibold()
                        .no_cursor()
                        .class("mq-on-primary"),
                )
                .on_click(move |_| {
                    submit_app.send(Command::ReplyForm {
                        asid: submitted.asid.clone(),
                        id: submitted.id.clone(),
                        answers: answers(&submitted),
                    });
                }),
        );
    let mut body = div().class("mq-approval-body").flex_col().child(top);
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
    card("mq-accent--warning", body)
}
