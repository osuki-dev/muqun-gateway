//! ACP form elicitation (`elicitation/create` with `mode: "form"`) as the
//! App's question form, and the answer back as the elicitation result.
//!
//! The requested schema is a flat object of primitive properties (the MCP
//! elicitation subset): a string, optionally with titled `oneOf` / `enum`
//! choices; a number or integer; a boolean; an array whose `items` carry the
//! choices (multi-select). Anything else is not shown: the request is
//! cancelled rather than half-answered.
//!
//! Claude Code's AskUserQuestion follows each choice question `K` with a
//! free-text `K_custom` ("Other"). For a single choice that pair is one App
//! field that also takes a typed answer: an offered choice goes back as `K`,
//! anything else as `K_custom`.
use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::agents::domain::{AgentSessionId, FormField, FormOption, FormRequest};
use crate::agents::ports::agent::AgentError;

const MAX_FIELDS: usize = 32;
const MAX_TEXT: usize = 4096;

pub(crate) struct Elicitation {
    pub rpc_id: Value,
    pub form: FormRequest,
    /// Single-choice key → its free-text companion key.
    custom: HashMap<String, String>,
}

pub(crate) fn form_id(rpc_id: &Value) -> Option<String> {
    match rpc_id {
        Value::String(id) if !id.is_empty() && id.len() <= 128 => Some(format!("elicitation:{id}")),
        Value::Number(id) => Some(format!("elicitation:{id}")),
        _ => None,
    }
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= MAX_TEXT)
        .map(str::to_owned)
}

fn choices(schema: &Value) -> Option<Vec<FormOption>> {
    if let Some(titled) = schema["oneOf"].as_array().or(schema["anyOf"].as_array()) {
        return titled
            .iter()
            .map(|choice| {
                let value = text(&choice["const"])?;
                Some(FormOption {
                    label: text(&choice["title"]).unwrap_or_else(|| value.clone()),
                    value,
                    description: text(&choice["description"]),
                })
            })
            .collect();
    }
    if let Some(plain) = schema["enum"].as_array() {
        let names = schema["enumNames"].as_array();
        return plain
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let value = text(value)?;
                Some(FormOption {
                    label: names
                        .and_then(|names| text(names.get(index)?))
                        .unwrap_or_else(|| value.clone()),
                    value,
                    description: None,
                })
            })
            .collect();
    }
    Some(Vec::new())
}

/// The App form for an `elicitation/create` request of `session`, or `None`
/// when it is not a form this mapping can show whole.
pub(crate) fn form(rpc_id: &Value, session: &str, params: &Value) -> Option<Elicitation> {
    if params["mode"].as_str().unwrap_or("form") != "form"
        || params["sessionId"].as_str() != Some(session)
    {
        return None;
    }
    let schema = &params["requestedSchema"];
    if schema["type"] != "object" {
        return None;
    }
    let properties = schema["properties"].as_object()?;
    if properties.is_empty() || properties.len() > MAX_FIELDS * 2 {
        return None;
    }
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut custom = HashMap::new();
    let mut fields = Vec::new();
    for (key, property) in properties {
        if key.is_empty() || key.len() > 128 {
            return None;
        }
        let companion = key
            .strip_suffix("_custom")
            .filter(|select| {
                properties[*select]["type"] == "string"
                    && choices(&properties[*select]).is_some_and(|c| !c.is_empty())
                    && property["type"] == "string"
            })
            .map(str::to_owned);
        if let Some(select) = companion {
            custom.insert(select, key.clone());
            continue;
        }
        let title = text(&property["title"]).unwrap_or_else(|| key.clone());
        let description = text(&property["description"]);
        let is_required = required.contains(&key.as_str());
        let field = match property["type"].as_str()? {
            "string" => FormField::String {
                key: key.clone(),
                title,
                description,
                required: is_required,
                when: vec![],
                placeholder: None,
                default: text(&property["default"]),
                options: choices(property)?,
                format: text(&property["format"]),
                min_length: property["minLength"].as_u64(),
                max_length: property["maxLength"].as_u64(),
                pattern: None,
                custom: false,
            },
            "number" | "integer" => FormField::Number {
                key: key.clone(),
                title,
                description,
                required: is_required,
                when: vec![],
                min: property["minimum"].as_f64(),
                max: property["maximum"].as_f64(),
                default: property["default"].as_f64(),
            },
            "boolean" => FormField::Boolean {
                key: key.clone(),
                title,
                description,
                required: is_required,
                when: vec![],
                default: property["default"].as_bool(),
            },
            "array" => {
                let options = choices(&property["items"])?;
                if options.is_empty() {
                    return None;
                }
                FormField::Multiselect {
                    key: key.clone(),
                    title,
                    description,
                    required: is_required,
                    when: vec![],
                    options,
                    default: vec![],
                }
            }
            _ => return None,
        };
        fields.push(field);
    }
    for field in &mut fields {
        if let FormField::String { key, custom: c, .. } = field {
            *c = custom.contains_key(key);
        }
    }
    if fields.is_empty() || fields.len() > MAX_FIELDS {
        return None;
    }
    Some(Elicitation {
        rpc_id: rpc_id.clone(),
        form: FormRequest {
            nonblocking: false,
            id: form_id(rpc_id)?,
            asid: AgentSessionId(session.into()),
            title: text(&params["message"]).unwrap_or_else(|| "Question".into()),
            fields,
        },
        custom,
    })
}

fn invalid(message: &str) -> AgentError {
    AgentError::InvalidRequest(message.into())
}

/// The elicitation result for the App's `answers`: `accept` with content
/// keyed as the schema asked. Unknown keys, missing required answers and
/// choices that were not offered are refused, nothing is sent.
pub(crate) fn answer(elicitation: &Elicitation, answers: &Value) -> Result<Value, AgentError> {
    let supplied = answers
        .as_object()
        .ok_or_else(|| invalid("answers must be an object"))?;
    let known = |key: &str| {
        elicitation
            .form
            .fields
            .iter()
            .any(|field| field_key(field) == key)
    };
    if let Some(unknown) = supplied.keys().find(|key| !known(key)) {
        return Err(AgentError::InvalidRequest(format!(
            "unexpected answer field {unknown}"
        )));
    }
    let mut content = Map::new();
    for field in &elicitation.form.fields {
        let key = field_key(field);
        let value = supplied
            .get(key)
            .filter(|value| !value.is_null() && value.as_str() != Some(""));
        let Some(value) = value else {
            if field_required(field) {
                return Err(AgentError::InvalidRequest(format!("{key} needs an answer")));
            }
            continue;
        };
        match field {
            FormField::String { options, .. } => {
                let value = value.as_str().ok_or_else(|| invalid("expected text"))?;
                if options.is_empty() || options.iter().any(|option| option.value == value) {
                    content.insert(key.into(), json!(value));
                } else if let Some(other) = elicitation.custom.get(key) {
                    content.insert(other.clone(), json!(value));
                } else {
                    return Err(invalid("answer is not an offered choice"));
                }
            }
            FormField::Number { .. } => {
                content.insert(
                    key.into(),
                    json!(value.as_f64().ok_or_else(|| invalid("expected a number"))?),
                );
            }
            FormField::Boolean { .. } => {
                content.insert(
                    key.into(),
                    json!(value
                        .as_bool()
                        .ok_or_else(|| invalid("expected true or false"))?),
                );
            }
            FormField::Multiselect { options, .. } => {
                let picks = value
                    .as_array()
                    .ok_or_else(|| invalid("expected a list"))?
                    .iter()
                    .map(|pick| {
                        pick.as_str()
                            .filter(|pick| options.iter().any(|option| option.value == *pick))
                            .map(|pick| json!(pick))
                            .ok_or_else(|| invalid("answer is not an offered choice"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                content.insert(key.into(), Value::Array(picks));
            }
            _ => return Err(invalid("unsupported field")),
        }
    }
    Ok(json!({ "action": "accept", "content": content }))
}

fn field_key(field: &FormField) -> &str {
    match field {
        FormField::String { key, .. }
        | FormField::Number { key, .. }
        | FormField::Boolean { key, .. }
        | FormField::Multiselect { key, .. }
        | FormField::External { key, .. }
        | FormField::Unknown { key, .. } => key,
    }
}

fn field_required(field: &FormField) -> bool {
    match field {
        FormField::String { required, .. }
        | FormField::Number { required, .. }
        | FormField::Boolean { required, .. }
        | FormField::Multiselect { required, .. } => *required,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request claude-agent-acp 0.99.1 sent for an AskUserQuestion (spike).
    fn ask_user_question() -> Value {
        json!({"mode":"form","sessionId":"s","toolCallId":"toolu_1","message":"Which fruit do you pick?",
            "requestedSchema":{"type":"object","properties":{
                "question_0":{"type":"string","title":"Fruit","oneOf":[
                    {"const":"Apple","title":"Apple","description":"Pick Apple"},
                    {"const":"Banana","title":"Banana","description":"Pick Banana"}]},
                "question_0_custom":{"type":"string","title":"Other","description":"Type your own answer"},
                "question_1":{"type":"array","title":"Extras","items":{"anyOf":[
                    {"const":"Nuts","title":"Nuts"},{"const":"Honey","title":"Honey"}]}},
                "question_1_custom":{"type":"string","title":"Other"}}}})
    }

    #[test]
    fn ask_user_question_maps_to_one_field_per_question_and_answers_back() {
        let elicitation = form(&json!(0), "s", &ask_user_question()).unwrap();
        assert_eq!(elicitation.form.id, "elicitation:0");
        assert_eq!(elicitation.form.title, "Which fruit do you pick?");
        let keys: Vec<_> = elicitation.form.fields.iter().map(field_key).collect();
        // The single choice absorbs its "Other"; the multi-select keeps it.
        assert_eq!(keys, ["question_0", "question_1", "question_1_custom"]);
        assert!(matches!(&elicitation.form.fields[0],
            FormField::String { custom: true, options, .. } if options.len() == 2));
        for (answers, expected) in [
            (
                json!({"question_0":"Banana"}),
                Ok(json!({"question_0":"Banana"})),
            ),
            (
                json!({"question_0":"Cherry"}),
                Ok(json!({"question_0_custom":"Cherry"})),
            ),
            (
                json!({"question_1":["Honey"],"question_1_custom":"Jam"}),
                Ok(json!({"question_1":["Honey"],"question_1_custom":"Jam"})),
            ),
            (json!({}), Ok(json!({}))),
            (json!({"question_1":["Jam"]}), Err(())),
            (json!({"question_9":"x"}), Err(())),
            (json!("Banana"), Err(())),
        ] {
            let result = answer(&elicitation, &answers)
                .map(|value| value["content"].clone())
                .map_err(|_| ());
            assert_eq!(result, expected, "{answers}");
        }
    }

    #[test]
    fn only_whole_forms_of_this_session_are_shown() {
        let schema = |properties: Value| {
            json!({"sessionId":"s","message":"m",
            "requestedSchema":{"type":"object","properties":properties,"required":["n"]}})
        };
        let shown = form(
            &json!("r"),
            "s",
            &schema(json!({
            "n":{"type":"integer","minimum":1},"b":{"type":"boolean"},
            "e":{"type":"string","enum":["x","y"],"enumNames":["X","Y"]}})),
        )
        .unwrap();
        assert!(matches!(&shown.form.fields[..], [
            FormField::Boolean { required: false, .. },
            FormField::String { options, custom: false, .. },
            FormField::Number { required: true, min: Some(_), .. }] if options[1].label == "Y"));
        assert!(answer(&shown, &json!({"b":true})).is_err(), "n is required");
        for (id, session, params) in [
            (json!("r"), "other", schema(json!({"n":{"type":"string"}}))),
            (json!("r"), "s", schema(json!({"n":{"type":"object"}}))),
            (json!("r"), "s", schema(json!({}))),
            (json!(null), "s", schema(json!({"n":{"type":"string"}}))),
            (
                json!("r"),
                "s",
                json!({"mode":"url","sessionId":"s","url":"https://example.invalid"}),
            ),
        ] {
            assert!(form(&id, session, &params).is_none(), "{params}");
        }
    }
}
