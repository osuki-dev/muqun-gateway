//! Questions the agent asks: the `question` tool card (what was asked and
//! what was picked, App `QuestionBody`) and the form that answers it
//! (App `agent-form-card`), inline in the timeline.

use muqun_gateway::agents::{FormField, FormOption, FormRequest, FormWhen, TimelineItem, ToolCall};
use rune::{col, icon, row, text, Cursor, El, TextEdit};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, Switch, TextInput};
use serde_json::{json, Map, Value};

use super::{caption, glyphs, prose, Tcx, ToolCard};
use crate::model::commands::Command;
use crate::model::tool_view::{self as tv, Output};
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

const QUESTION_MAX: usize = 6;
const OPTION_MAX: usize = 8;

/// The pending form this session is waiting on that no `Form` row of the
/// timeline shows already: the answer to a live question card.
fn unshown_form<'a>(cx: &Tcx<'a>) -> Option<&'a FormRequest> {
    let forms = cx.s.waiting_forms.get(cx.asid)?;
    let tl = cx.tl();
    forms.iter().find(|f| {
        !tl.is_some_and(|t| {
            t.items.iter().any(|i| {
                matches!(&i.part, muqun_gateway::agents::AgentPart::Form { request } if request.id == f.id)
            })
        })
    })
}

/// A `question` tool call: live, the form to answer it; answered, the
/// questions with the chosen options marked.
pub fn question_card(
    cx: &Tcx,
    _item: &TimelineItem,
    call: &ToolCall,
    rec: Option<&Map<String, Value>>,
    out: &Output,
    card: &mut ToolCard,
) {
    let questions = rec.map(tv::parse_questions).unwrap_or_default();
    let answers = tv::question_answers(call);
    card.accent = Some(cx.k.c.icon_accent);
    if tv::pending(call.state) && !cx.stopped() {
        if let Some(form) = unshown_form(cx) {
            card.preview = Some(form_fields(cx.s, cx.k, form, true));
            card.attention = true;
            return;
        }
        card.open_default = true;
    }
    if questions.is_empty() {
        if !out.stdout.trim().is_empty() {
            card.body = Some(prose(cx, "answer", &out.stdout));
        }
        return;
    }
    let k = cx.k;
    let mut body = col().key("questions").w_full().gap(12.0);
    for (qi, q) in questions.iter().take(QUESTION_MAX).enumerate() {
        let picked = answers.get(qi).cloned().unwrap_or_default();
        let mut block = col().key(format!("q{qi}")).w_full().gap(6.0);
        let mut head = row().key("h").gap(8.0).center_y();
        let titled = questions.len() == 1 && q.header == card.title;
        if !q.header.is_empty() && !titled {
            head = head.child(
                text(q.header.clone())
                    .font(k.ty.small_medium)
                    .color(k.c.text_base),
            );
        }
        if q.multiple {
            head = head.child(caption(k, cx.t("Several answers", "可多选")));
        }
        block = block.child(head);
        if !q.question.is_empty() {
            block = block.child(prose(cx, &format!("q{qi}"), &q.question));
        }
        let mut opts = row().key("opts").w_full().gap(6.0).flex_wrap();
        for (oi, o) in q.options.iter().take(OPTION_MAX).enumerate() {
            let chosen = picked.contains(&o.label);
            opts = opts.child(option_pill(
                k,
                format!("o{oi}"),
                &o.label,
                o.description.as_deref(),
                chosen,
            ));
        }
        block = block.child(opts);
        let unlisted: Vec<&String> = picked
            .iter()
            .filter(|a| !q.options.iter().any(|o| &o.label == *a))
            .collect();
        if !unlisted.is_empty() {
            block = block.child(
                text(
                    unlisted
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" · "),
                )
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap(),
            );
        }
        body = body.child(block);
    }
    card.body = Some(body);
}

/// An option as a pill: its label, its description under it, and a mark
/// when it is (or was) the answer.
fn option_pill(k: &Kit, key: String, label: &str, description: Option<&str>, chosen: bool) -> El {
    let ink = if chosen {
        k.c.text_accent
    } else {
        k.c.text_base
    };
    let mut copy = col().gap(1.0).shrink().child(
        text(label.to_string())
            .font(if chosen {
                k.ty.small_medium
            } else {
                k.ty.small
            })
            .color(ink)
            .wrap(),
    );
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        copy = copy.child(
            text(d.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint)
                .wrap(),
        );
    }
    let mut pill = row()
        .key(key)
        .gap(6.0)
        .px(10.0)
        .py(5.0)
        .radius(k.radius.md)
        .border(
            1.0,
            if chosen {
                k.c.border_focus
            } else {
                k.c.border_base
            },
        )
        .max_w(360.0);
    if chosen {
        pill = pill
            .fill(k.alpha(k.c.icon_accent, 0.12))
            .child(icon(&glyphs::DONE, 12.0).color(k.c.icon_accent).mt(2.0));
    }
    pill.child(copy)
}

// ---- forms ------------------------------------------------------------------

fn form_key(form: &str, field: &str) -> String {
    format!("{form}/{field}")
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

fn field_when(field: &FormField) -> &[FormWhen] {
    match field {
        FormField::String { when, .. }
        | FormField::Number { when, .. }
        | FormField::Boolean { when, .. }
        | FormField::Multiselect { when, .. }
        | FormField::External { when, .. }
        | FormField::Unknown { when, .. } => when,
    }
}

fn field_required(field: &FormField) -> bool {
    match field {
        FormField::String { required, .. }
        | FormField::Number { required, .. }
        | FormField::Boolean { required, .. }
        | FormField::Multiselect { required, .. } => *required,
        FormField::External { .. } | FormField::Unknown { .. } => false,
    }
}

fn form_value(s: &UiState, form: &str, field: &FormField) -> String {
    let (key, default) = field_meta(field);
    s.form_inputs
        .get(&form_key(form, key))
        .map_or(default, |e| e.text.clone())
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
    let mut answers = Map::new();
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

/// A required visible field still empty: the form can't be sent yet.
fn missing_required(s: &UiState, form: &FormRequest) -> bool {
    form.fields
        .iter()
        .filter(|f| field_required(f) && visible(s, form, f))
        .any(|f| {
            !matches!(f, FormField::Boolean { .. }) && form_value(s, &form.id, f).trim().is_empty()
        })
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

fn text_input(s: &UiState, k: &Kit, slot: &str, current: &str, placeholder: &str) -> El {
    let edit = s
        .form_inputs
        .get(slot)
        .cloned()
        .unwrap_or_else(|| TextEdit::new(current.to_string()));
    let (target, current) = (slot.to_string(), current.to_string());
    TextInput::new(&edit)
        .placeholder(placeholder)
        .on_edit(move |s: &mut UiState| {
            s.form_inputs
                .entry(target.clone())
                .or_insert_with(|| TextEdit::new(current.clone()))
        })
        .build(k)
        .key(format!("in-{slot}"))
}

fn field_input(s: &UiState, k: &Kit, form: &FormRequest, field: &FormField) -> El {
    let language = s.cfg.general.language;
    let (key, _) = field_meta(field);
    let slot = form_key(&form.id, key);
    let current = form_value(s, &form.id, field);
    let none: &[FormOption] = &[];
    let (title, description, options, multi, custom) = match field {
        FormField::String {
            title,
            description,
            options,
            custom,
            ..
        } => (
            title,
            description.as_ref(),
            options.as_slice(),
            false,
            *custom,
        ),
        FormField::Multiselect {
            title,
            description,
            options,
            ..
        } => (title, description.as_ref(), options.as_slice(), true, false),
        FormField::Number {
            title, description, ..
        }
        | FormField::Boolean {
            title, description, ..
        }
        | FormField::External {
            title, description, ..
        } => (title, description.as_ref(), none, false, false),
        FormField::Unknown { title, .. } => (title, None, none, false, false),
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
        FormField::External { url, .. } => crate::ui::muted(k, url.clone()),
        FormField::Unknown { .. } => crate::ui::muted(
            k,
            tr_ref(
                language,
                "This question needs a newer app to answer.",
                "需要更新版本的应用才能回答此问题。",
            ),
        ),
        _ if !options.is_empty() => {
            let chosen: Vec<&str> = current.split(',').collect();
            let mut list = col().key(format!("opts-{slot}")).w_full().gap(6.0);
            let mut line = row().key("pills").w_full().gap(6.0).flex_wrap();
            for option in options {
                let on = chosen.contains(&option.value.as_str());
                line = line.child(
                    k.interactive(
                        row()
                            .key(format!("opt-{slot}-{}", option.value))
                            .radius(k.radius.md)
                            .child(option_pill(
                                k,
                                "pill".into(),
                                &option.label,
                                option.description.as_deref(),
                                on,
                            )),
                    )
                    .cursor(Cursor::Pointer)
                    .label(option.label.clone())
                    .on_press(pick(&form.id, key, option.value.clone(), multi)),
                );
            }
            list = list.child(line);
            if custom {
                let typed = if options.iter().any(|o| o.value == current) {
                    String::new()
                } else {
                    current.clone()
                };
                list = list.child(text_input(
                    s,
                    k,
                    &slot,
                    &typed,
                    tr_ref(language, "Or type an answer", "或输入回答"),
                ));
            }
            list
        }
        FormField::String { placeholder, .. } => text_input(
            s,
            k,
            &slot,
            &current,
            placeholder
                .as_deref()
                .unwrap_or(tr_ref(language, "Type here", "在此输入")),
        ),
        _ => text_input(s, k, &slot, &current, title),
    };
    let mut block = col().key(format!("field-{slot}")).w_full().gap(6.0);
    if !matches!(field, FormField::Boolean { .. }) {
        block = block.child(
            text(title.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_base)
                .wrap(),
        );
    }
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        block = block.child(crate::ui::muted(k, d.clone()));
    }
    block.child(control)
}

/// A form's fields and its Submit, without a card around them.
pub fn form_fields(s: &UiState, k: &Kit, form: &FormRequest, compact: bool) -> El {
    let language = s.cfg.general.language;
    let mut body = col()
        .key(format!("form-{}", form.id))
        .w_full()
        .gap(if compact { 10.0 } else { 12.0 });
    for field in form.fields.iter().filter(|f| visible(s, form, f)) {
        body = body.child(field_input(s, k, form, field));
    }
    let (asid, id) = (form.asid.clone(), form.id.clone());
    let blocked = missing_required(s, form);
    body.child(
        row().key("submit-row").w_full().child(
            Button::new(tr_ref(language, "Submit", "提交"))
                .variant(ButtonVariant::Submit)
                .control_size(ControlSize::Small)
                .disabled(blocked)
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
        ),
    )
}

fn form_pending(s: &UiState, form: &FormRequest) -> bool {
    s.waiting_forms
        .get(&form.asid.0)
        .is_some_and(|l| l.iter().any(|f| f.id == form.id))
}

/// A form as a card: its title, then its fields while it waits, or a quiet
/// "Answered" line once it has been.
pub fn form_card(s: &UiState, k: &Kit, form: &FormRequest) -> El {
    let language = s.cfg.general.language;
    let pending = form_pending(s, form);
    let accent = if pending {
        k.c.icon_accent
    } else {
        k.c.icon_faint
    };
    let head = row()
        .key("head")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(icon(&glyphs::CHOICE, 15.0).color(accent))
        .child(
            col()
                .gap(1.0)
                .shrink()
                .grow(1.0)
                .child(
                    text(form.title.clone())
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .wrap(),
                )
                .child(
                    text(if pending {
                        tr_ref(language, "The agent is asking you", "Agent 在向你提问")
                    } else {
                        tr_ref(language, "Answered", "已回答")
                    })
                    .font(k.ty.small)
                    .color(k.c.text_faint),
                ),
        );
    let mut card = col()
        .key(format!("formcard-{}", form.id))
        .w_full()
        .gap(12.0)
        .px(14.0)
        .py(12.0)
        .radius(k.radius.lg)
        .border(
            1.0,
            if pending {
                k.alpha(k.c.icon_accent, 0.45)
            } else {
                k.c.border_weak
            },
        )
        .fill(k.c.surface_raised_base)
        .child(head);
    if pending {
        card = card.child(form_fields(s, k, form, false));
    }
    card
}
