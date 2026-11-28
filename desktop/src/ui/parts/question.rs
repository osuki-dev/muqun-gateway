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

/// Pending questions may arrive as events without a timeline Form item.
/// Keep those visible above the composer, independent of tool folding/state.
pub fn pending_forms<'a>(s: &'a UiState, asid: &str) -> Vec<&'a FormRequest> {
    let Some(forms) = s.waiting_forms.get(asid) else {
        return Vec::new();
    };
    let timeline = s.timelines.get(asid);
    forms
        .iter()
        .filter(|form| {
            !timeline.is_some_and(|t| {
                t.items.iter().any(|item| {
                    matches!(&item.part,
            muqun_gateway::agents::AgentPart::Form { request } if request.id == form.id)
                })
            })
        })
        .collect()
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
    card.accent = Some(cx.k.c.info.icon);
    if tv::pending(call.state) && !cx.stopped() {
        card.open_default = true;
    } else if !answers.is_empty() {
        let summary: Vec<String> = answers
            .iter()
            .filter(|a| !a.is_empty())
            .map(|a| a.join(", "))
            .collect();
        if !summary.is_empty() {
            card.caption = summary.join(" · ");
        }
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
            head = head.child(caption(k, cx.t("Several answers")));
        }
        block = block.child(head);
        if !q.question.is_empty() {
            block = block.child(prose(cx, &format!("q{qi}"), &q.question));
        }
        let mut opts = row()
            .key("opts")
            .w_full()
            .gap(6.0)
            .flex_wrap()
            .items_start();
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
        for (ui, custom) in picked
            .iter()
            .filter(|a| !q.options.iter().any(|o| &o.label == *a))
            .enumerate()
        {
            opts = opts.child(option_pill(k, format!("u{ui}"), custom, None, true));
        }
        block = block.child(opts);
        body = body.child(block);
    }
    card.body = Some(body);
}

/// An option as a pill: its label, its description under it, and a mark
/// when it is (or was) the answer.
fn option_pill(k: &Kit, key: String, label: &str, description: Option<&str>, chosen: bool) -> El {
    let ink = k.c.text_base;
    let desc_ink = if chosen {
        k.c.text_muted
    } else {
        k.c.text_faint
    };
    let has_desc = description.is_some_and(|d| !d.is_empty());
    let mut copy = col().gap(2.0).shrink().child(
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
        copy = copy.child(text(d.to_string()).font(k.ty.small).color(desc_ink).wrap());
    }
    let [c0, c1, _] = crate::theme::look::highlight_colors_for(k, k.dark);
    let mut pill = row()
        .key(key)
        .gap(6.0)
        .px(10.0)
        .py(6.0)
        .min_h(30.0)
        .radius(k.radius.md)
        .max_w(360.0)
        .token_motion(k.motion.disclosure);
    pill = if has_desc {
        pill.items_start()
    } else {
        pill.center_y()
    };
    if chosen {
        let mut mark = icon(&glyphs::DONE, 12.0).color(c0);
        if has_desc {
            mark = mark.mt(2.0);
        }
        pill = pill
            .border(1.0, c0.alpha(0.60))
            .linear_gradient(
                std::f32::consts::FRAC_PI_2,
                [(0.0, c0.alpha(0.20)), (1.0, c1.alpha(0.13))],
            )
            .child(mark);
    } else {
        pill = pill
            .border(1.0, k.c.border_weak)
            .fill(k.c.layer_01.alpha(if k.dark { 0.55 } else { 0.92 }));
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
    let custom_slot = format!("{slot}#custom");
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
        s.form_inputs.remove(&custom_slot);
        s.form_inputs.insert(slot.clone(), TextEdit::new(next));
        s.draft.focused = false;
    }
}

fn submit_form(s: &mut UiState, form: &FormRequest) -> bool {
    if missing_required(s, form) {
        return false;
    }
    let answers = form_answers(s, form);
    if let Some(list) = s.waiting_forms.get_mut(&form.asid.0) {
        list.retain(|f| f.id != form.id);
    }
    s.send(Command::ReplyForm {
        asid: form.asid.clone(),
        id: form.id.clone(),
        answers,
    });
    s.draft.focused = true;
    true
}

/// Submit the first ready pending question form for the active session.
/// Called when the user presses Enter after picking an option pill.
pub fn submit_active_form(s: &mut UiState) -> bool {
    let Some(asid) = s.selected.clone() else {
        return false;
    };
    let form = s
        .waiting_forms
        .get(&asid)
        .and_then(|forms| forms.iter().find(|f| !missing_required(s, f)))
        .cloned();
    let Some(form) = form else {
        return false;
    };
    submit_form(s, &form)
}

fn insert_pasted_paths(edit: &mut TextEdit, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let joined = paths.join(" ");
    let prefix = if !edit.text.is_empty() && !edit.text.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let appended = format!("{}{prefix}{joined}", edit.text);
    let len = appended.len();
    edit.text = appended;
    edit.anchor = len;
    edit.focus = len;
}

/// Handle clipboard file or image pastes inside a question answer field.
/// Because agent `ReplyForm` payloads carry JSON text answers (not binary
/// attachments), pasted screenshots are saved to local uploads and their path
/// (or pasted file paths) are inserted so the agent can read them directly.
fn paste_into_form_slot(
    s: &mut UiState,
    edit_slot: &str,
    answer_slot: &str,
    content: &rune::ClipboardContent,
) -> bool {
    let lang = s.cfg.general.language;
    let files = content.files();
    let paths: Vec<String> = if !files.is_empty() {
        files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    } else if let Some(image) = content.image() {
        match muqun_gateway::platform::store_upload("question-clipboard.png", image.png()) {
            Ok(stored) => vec![stored.path.to_string_lossy().into_owned()],
            Err(err) => {
                s.toast_error(
                    tr_ref(lang, "Could not save pasted image"),
                    Some(err.message()),
                );
                return true;
            }
        }
    } else {
        return false;
    };
    if paths.is_empty() {
        return false;
    }
    let entry = s
        .form_inputs
        .entry(edit_slot.to_string())
        .or_insert_with(|| TextEdit::new(""));
    insert_pasted_paths(entry, &paths);
    let updated = entry.text.clone();
    if answer_slot != edit_slot {
        s.form_inputs
            .insert(answer_slot.to_string(), TextEdit::new(updated));
    }
    let msg = if content.image().is_some() {
        tr_ref(lang, "Pasted image path into answer")
    } else {
        tr_ref(lang, "Inserted file path into answer")
    };
    s.toast(msg.to_string(), None);
    true
}

fn text_input(
    s: &UiState,
    k: &Kit,
    form: &FormRequest,
    slot: &str,
    current: &str,
    placeholder: &str,
) -> El {
    let edit = s
        .form_inputs
        .get(slot)
        .cloned()
        .unwrap_or_else(|| TextEdit::new(current.to_string()));
    let (target, current) = (slot.to_string(), current.to_string());
    let paste_slot = slot.to_string();
    let submit_target = form.clone();
    TextInput::new(&edit)
        .placeholder(placeholder)
        .on_edit(move |s: &mut UiState| {
            s.form_inputs
                .entry(target.clone())
                .or_insert_with(|| TextEdit::new(current.clone()))
        })
        .on_input_paste(
            move |s: &mut UiState, content: &rune::ClipboardContent, _: &mut rune::Tx| {
                paste_into_form_slot(s, &paste_slot, &paste_slot, content)
            },
        )
        .on_key(
            move |s: &mut UiState, ev: &rune::KeyEvent, _: &mut rune::Tx| {
                if ev.key == rune::Key::Enter
                    && !(ev.mods.shift
                        || ev.mods.ctrl
                        || ev.mods.cmd
                        || ev.mods.alt
                        || ev.mods.logo)
                {
                    return submit_form(s, &submit_target);
                }
                false
            },
        )
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
                    s.draft.focused = false;
                }
            })
            .build(k),
        FormField::External { url, .. } => crate::ui::muted(k, url.clone()),
        FormField::Unknown { .. } => crate::ui::muted(
            k,
            tr_ref(language, "This question needs a newer app to answer."),
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
                let custom_slot = format!("{slot}#custom");
                let typed = s
                    .form_inputs
                    .get(&custom_slot)
                    .map(|e| e.text.clone())
                    .unwrap_or_else(|| {
                        if options.iter().any(|o| o.value == current) {
                            String::new()
                        } else {
                            current.clone()
                        }
                    });
                let edit = s
                    .form_inputs
                    .get(&custom_slot)
                    .cloned()
                    .unwrap_or_else(|| TextEdit::new(typed.clone()));
                let (edit_slot, answer_slot, init_typed) =
                    (custom_slot.clone(), slot.clone(), typed);
                let paste_edit = custom_slot.clone();
                let paste_answer = slot.clone();
                let sync_slot = custom_slot.clone();
                let sync_answer = slot.clone();
                let submit_target = form.clone();
                list = list.child(
                    TextInput::new(&edit)
                        .placeholder(tr_ref(language, "Or type an answer"))
                        .on_edit(move |s: &mut UiState| {
                            s.form_inputs
                                .entry(edit_slot.clone())
                                .or_insert_with(|| TextEdit::new(init_typed.clone()))
                        })
                        .on_input_paste(
                            move |s: &mut UiState,
                                  content: &rune::ClipboardContent,
                                  _: &mut rune::Tx| {
                                paste_into_form_slot(s, &paste_edit, &paste_answer, content)
                            },
                        )
                        .on_change(move |s: &mut UiState, _: &mut rune::Tx| {
                            let val = s
                                .form_inputs
                                .get(&sync_slot)
                                .map(|e| e.text.clone())
                                .unwrap_or_default();
                            s.form_inputs
                                .insert(sync_answer.clone(), TextEdit::new(val));
                        })
                        .on_key(
                            move |s: &mut UiState, ev: &rune::KeyEvent, _: &mut rune::Tx| {
                                if ev.key == rune::Key::Enter
                                    && !(ev.mods.shift
                                        || ev.mods.ctrl
                                        || ev.mods.cmd
                                        || ev.mods.alt
                                        || ev.mods.logo)
                                {
                                    return submit_form(s, &submit_target);
                                }
                                false
                            },
                        )
                        .build(k)
                        .key(format!("in-{answer_slot}")),
                );
            }
            list
        }
        FormField::String { placeholder, .. } => text_input(
            s,
            k,
            form,
            &slot,
            &current,
            placeholder
                .as_deref()
                .unwrap_or(tr_ref(language, "Type here")),
        ),
        _ => text_input(s, k, form, &slot, &current, title),
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
    let blocked = missing_required(s, form);
    let mut submit_row = row().key("submit-row").w_full().center_y().gap(8.0).child(
        Button::new(tr_ref(language, "Submit"))
            .variant(ButtonVariant::Submit)
            .control_size(ControlSize::Regular)
            .shortcut("↵")
            .disabled(blocked)
            .on_press({
                let form = form.clone();
                move |s: &mut UiState| {
                    submit_form(s, &form);
                }
            })
            .build(k)
            .key(format!("form-submit-{}", form.id)),
    );
    if !blocked {
        submit_row = submit_row.child(
            row()
                .gap(4.0)
                .center_y()
                .child(crate::ui::shortcuts::keycaps(k, &["↵"]))
                .child(
                    text(tr_ref(language, "Enter to submit"))
                        .font(k.ty.caption())
                        .color(k.c.text_faint),
                ),
        );
    }
    body.child(submit_row)
}

fn form_pending(s: &UiState, form: &FormRequest) -> bool {
    s.waiting_forms
        .get(&form.asid.0)
        .is_some_and(|l| l.iter().any(|f| f.id == form.id))
}

/// Compact summary of what the owner answered on a resolved form, when known.
fn answered_summary(s: &UiState, k: &Kit, form: &FormRequest) -> Option<El> {
    let mut pills = row()
        .key("answered-pills")
        .w_full()
        .gap(6.0)
        .flex_wrap()
        .items_start();
    let mut any = false;
    for field in form.fields.iter().filter(|f| visible(s, form, f)) {
        let (key, _) = field_meta(field);
        let slot = form_key(&form.id, key);
        let Some(raw) = s
            .form_inputs
            .get(&slot)
            .map(|e| e.text.trim())
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let label = match field {
            FormField::String { options, .. } => options
                .iter()
                .find(|o| o.value == raw)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| raw.to_string()),
            FormField::Multiselect { options, .. } => raw
                .split(',')
                .filter(|v| !v.is_empty())
                .map(|v| {
                    options
                        .iter()
                        .find(|o| o.value == v)
                        .map(|o| o.label.as_str())
                        .unwrap_or(v)
                })
                .collect::<Vec<_>>()
                .join(", "),
            FormField::Boolean { title, .. } => {
                format!("{title}: {}", if raw == "true" { "On" } else { "Off" })
            }
            _ => raw.to_string(),
        };
        if label.is_empty() {
            continue;
        }
        pills = pills.child(option_pill(k, format!("ans-{key}"), &label, None, true));
        any = true;
    }
    any.then_some(pills)
}

/// A form as a card: its title, then its fields while it waits, or a quiet
/// "Answered" line (plus the recorded answer pills when present) once it has been.
pub fn form_card(s: &UiState, k: &Kit, form: &FormRequest) -> El {
    let language = s.cfg.general.language;
    let pending = form_pending(s, form);
    let accent = if pending {
        k.c.info.icon
    } else {
        k.c.success.icon
    };
    let head_icon = if pending {
        &glyphs::CHOICE
    } else {
        &glyphs::DONE
    };
    let head = row()
        .key("head")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(icon(head_icon, 15.0).color(accent))
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
                        tr_ref(language, "The agent is asking you")
                    } else {
                        tr_ref(language, "Answered")
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
        .clip_rounded(k.radius.lg)
        .border(1.0, k.c.border_weak)
        // Waiting on the owner: the surface a faint step toward its tone.
        .fill(if pending {
            s.look().emphasis(k.c.surface_raised_base, k.c.info.icon)
        } else {
            k.c.surface_raised_base
        })
        .child(head);
    if pending {
        card = card.child(form_fields(s, k, form, false));
    } else if let Some(summary) = answered_summary(s, k, form) {
        card = card.child(summary);
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::FormRequest;

    fn sample_form(required: bool) -> FormRequest {
        serde_json::from_value(serde_json::json!({
            "asid": "s-1",
            "id": "f-1",
            "title": "Choose deployment target",
            "fields": [{
                "type": "string",
                "key": "target",
                "title": "Target",
                "required": required,
                "placeholder": "staging or prod",
                "options": [
                    { "value": "staging", "label": "Staging" },
                    { "value": "prod", "label": "Production" }
                ],
                "custom": true
            }]
        }))
        .unwrap()
    }

    #[test]
    fn enter_submits_ready_form_and_ignores_missing_required() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        while rx.try_recv().is_ok() {}
        s.selected = Some("s-1".into());
        s.waiting_forms
            .insert("s-1".into(), vec![sample_form(true)]);

        // Required field is still empty -> must NOT submit.
        assert!(!submit_active_form(&mut s));
        assert!(rx.try_recv().is_err());
        assert_eq!(s.waiting_forms["s-1"].len(), 1);

        // Selecting an option satisfies the required field -> Enter submits immediately.
        pick("f-1", "target", "staging".into(), false)(&mut s);
        assert!(submit_active_form(&mut s));
        assert!(s.waiting_forms["s-1"].is_empty());

        let cmd = rx.try_recv().expect("expected ReplyForm command");
        match cmd {
            Command::ReplyForm { asid, id, answers } => {
                assert_eq!(asid.0, "s-1");
                assert_eq!(id, "f-1");
                assert_eq!(answers["target"], serde_json::json!("staging"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn window_enter_submits_after_option_pick() {
        use rune::{Key, KeyEvent, Mods, Tx};

        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        while rx.try_recv().is_ok() {}
        s.mode = crate::ui::Mode::Session;
        s.selected = Some("s-1".into());
        s.waiting_forms
            .insert("s-1".into(), vec![sample_form(true)]);
        pick("f-1", "target", "prod".into(), false)(&mut s);

        let ev = KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        };
        let mut rtx = Tx::default();
        assert!(crate::ui::keymap::dispatch_window(&mut s, &ev, &mut rtx));
        assert!(s.waiting_forms["s-1"].is_empty());

        let cmd = rx.try_recv().expect("expected ReplyForm command");
        assert!(matches!(
            cmd,
            Command::ReplyForm { ref id, ref answers, .. }
                if id == "f-1" && answers["target"] == serde_json::json!("prod")
        ));
    }

    #[test]
    fn pasted_paths_append_cleanly_to_question_answer() {
        let mut edit = TextEdit::new("see");
        insert_pasted_paths(
            &mut edit,
            &["/tmp/a.png".to_string(), "/tmp/b.log".to_string()],
        );
        assert_eq!(edit.text, "see /tmp/a.png /tmp/b.log");
        assert_eq!(edit.anchor, edit.text.len());
        assert_eq!(edit.focus, edit.text.len());
    }
}
