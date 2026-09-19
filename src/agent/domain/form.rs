use serde::{Deserialize, Serialize};
use super::session::AgentSessionId;

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormOption {
    pub value: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `Form.When`: a field is only shown when every condition holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormWhen {
    pub key: String,
    /// `eq` or `neq`.
    pub op: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FormField {
    String {
        key: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        when: Vec<FormWhen>,
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<FormOption>,
        /// `email`, `uri`, `date` or `date-time`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_length: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_length: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        /// The field accepts a value outside `options`.
        #[serde(default, skip_serializing_if = "is_false")]
        custom: bool,
    },
    Number {
        key: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        when: Vec<FormWhen>,
        #[serde(skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
    },
    Boolean {
        key: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        when: Vec<FormWhen>,
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Multiselect {
        key: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default)]
        required: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        when: Vec<FormWhen>,
        options: Vec<FormOption>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        default: Vec<String>,
    },
    External {
        key: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        when: Vec<FormWhen>,
        url: String,
    },
    Unknown {
        key: String,
        title: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        when: Vec<FormWhen>,
        raw_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormRequest {
    pub id: String,
    pub asid: AgentSessionId,
    pub title: String,
    pub fields: Vec<FormField>,
}
