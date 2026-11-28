//! Settings → MCP Hub: manage external upstream MCP servers hosted once in
//! Muqun Gateway and exposed to every connected agent via Tool Search
//! (`hub.search_tools` / `hub.call_tool`).

use std::collections::{BTreeMap, HashSet};

use rune::{col, el, row as hrow, text, El, TextEdit};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, TextInput};

use super::{button, choice, empty, field, flag, group, group_with, row, status, Tone};
use crate::hub::{
    McpBuiltinView, McpHubError, McpHubSnapshot, McpToolCallResult, McpUpstreamToolPreview,
    McpUpstreamTransport, McpUpstreamUpsert, McpUpstreamView,
};
use crate::model::commands::Command;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

#[cfg(test)]
const MASKED_SECRET: &str = "••••••••";
const SCOPES: [&str; 3] = ["local_only", "tasks", "all"];
const ISOLATIONS: [&str; 2] = ["shared", "per_session"];
const MAX_PREVIEW_CHARS: usize = 4000;

/// Built-in quick-start templates for the "Add MCP server" form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTemplate {
    Filesystem,
    GitHub,
    Context7,
    Playwright,
    TailscaleHttp,
}

const TEMPLATES: [McpTemplate; 5] = [
    McpTemplate::Filesystem,
    McpTemplate::GitHub,
    McpTemplate::Context7,
    McpTemplate::Playwright,
    McpTemplate::TailscaleHttp,
];

impl McpTemplate {
    fn key(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::GitHub => "github",
            Self::Context7 => "context7",
            Self::Playwright => "playwright",
            Self::TailscaleHttp => "tailscale-http",
        }
    }

    fn button_label(self, lang: crate::config::Language) -> &'static str {
        match self {
            Self::Filesystem => tr_ref(lang, "Filesystem"),
            Self::GitHub => "GitHub",
            Self::Context7 => "Context7",
            Self::Playwright => "Playwright",
            Self::TailscaleHttp => tr_ref(lang, "Tailscale HTTP"),
        }
    }
}

/// Outcome of the last live tool test invocation shown in the inspector panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallPanelResult {
    pub server_id: String,
    pub tool: String,
    pub outcome: Result<McpToolCallResult, String>,
}

pub struct McpHubState {
    pub builtin: Option<McpBuiltinView>,
    pub upstreams: Vec<McpUpstreamView>,
    pub loading: bool,
    pub unsupported_reason: Option<String>,
    pub error: Option<String>,
    pub busy_ids: HashSet<String>,
    pub confirm_delete: Option<String>,
    pub form_open: bool,
    pub editing_id: Option<String>,
    pub id_input: TextEdit,
    pub label_input: TextEdit,
    /// 0 = Streamable HTTP, 1 = Stdio command
    pub transport_idx: usize,
    pub url_input: TextEdit,
    pub auth_header_input: TextEdit,
    pub command_input: TextEdit,
    pub args_input: TextEdit,
    pub env_input: TextEdit,
    /// 0 = local_only, 1 = tasks, 2 = all
    pub scope_idx: usize,
    /// 0 = shared, 1 = per_session
    pub isolation_idx: usize,
    pub timeout_input: TextEdit,
    pub allowed_tools_input: TextEdit,
    pub denied_tools_input: TextEdit,
    pub form_saving: bool,
    pub form_error: Option<String>,
    /// Currently inspected tool chip `(server_id, tool_name)`.
    pub inspected_tool: Option<(String, String)>,
    /// Whether the raw JSON schema block is expanded in the inspector panel.
    pub show_raw_schema: bool,
    /// JSON arguments input for the interactive live tool tester.
    pub tool_args_input: TextEdit,
    /// Validation error for `tool_args_input` before sending a request.
    pub tool_args_error: Option<String>,
    /// In-flight live tool test `(server_id, tool_name)`.
    pub tool_calling: Option<(String, String)>,
    /// Last live tool test result or error.
    pub tool_call_output: Option<ToolCallPanelResult>,
}

impl Default for McpHubState {
    fn default() -> Self {
        Self {
            builtin: None,
            upstreams: Vec::new(),
            loading: false,
            unsupported_reason: None,
            error: None,
            busy_ids: HashSet::new(),
            confirm_delete: None,
            form_open: false,
            editing_id: None,
            id_input: TextEdit::new(""),
            label_input: TextEdit::new(""),
            transport_idx: 0,
            url_input: TextEdit::new(""),
            auth_header_input: TextEdit::new(""),
            command_input: TextEdit::new(""),
            args_input: TextEdit::new(""),
            env_input: TextEdit::new(""),
            scope_idx: 0,
            isolation_idx: 0,
            timeout_input: TextEdit::new("30"),
            allowed_tools_input: TextEdit::new(""),
            denied_tools_input: TextEdit::new(""),
            form_saving: false,
            form_error: None,
            inspected_tool: None,
            show_raw_schema: false,
            tool_args_input: TextEdit::new("{}"),
            tool_args_error: None,
            tool_calling: None,
            tool_call_output: None,
        }
    }
}

impl McpHubState {
    pub fn reset_for_switch(&mut self) {
        self.builtin = None;
        self.upstreams.clear();
        self.loading = false;
        self.unsupported_reason = None;
        self.error = None;
        self.busy_ids.clear();
        self.confirm_delete = None;
        self.inspected_tool = None;
        self.show_raw_schema = false;
        self.tool_args_input = TextEdit::new("{}");
        self.tool_args_error = None;
        self.tool_calling = None;
        self.tool_call_output = None;
        self.close_form();
    }

    pub fn apply_template(&mut self, template: McpTemplate) {
        self.form_open = true;
        self.editing_id = None;
        self.scope_idx = 0; // local_only
        self.isolation_idx = 0; // shared
        self.timeout_input = TextEdit::new("30");
        self.allowed_tools_input = TextEdit::new("");
        self.denied_tools_input = TextEdit::new("");
        self.form_error = None;
        match template {
            McpTemplate::Filesystem => {
                self.id_input = TextEdit::new("filesystem");
                self.label_input = TextEdit::new("Filesystem");
                self.transport_idx = 1;
                self.url_input = TextEdit::new("");
                self.auth_header_input = TextEdit::new("");
                self.command_input = TextEdit::new("npx");
                self.args_input =
                    TextEdit::new("-y @modelcontextprotocol/server-filesystem /path/to/dir");
                self.env_input = TextEdit::new("");
            }
            McpTemplate::GitHub => {
                self.id_input = TextEdit::new("github");
                self.label_input = TextEdit::new("GitHub");
                self.transport_idx = 1;
                self.url_input = TextEdit::new("");
                self.auth_header_input = TextEdit::new("");
                self.command_input = TextEdit::new("npx");
                self.args_input = TextEdit::new("-y @modelcontextprotocol/server-github");
                self.env_input = TextEdit::new("GITHUB_PERSONAL_ACCESS_TOKEN=");
            }
            McpTemplate::Context7 => {
                self.id_input = TextEdit::new("context7");
                self.label_input = TextEdit::new("Context7");
                self.transport_idx = 1;
                self.url_input = TextEdit::new("");
                self.auth_header_input = TextEdit::new("");
                self.command_input = TextEdit::new("npx");
                self.args_input = TextEdit::new("-y @upstash/context7-mcp");
                self.env_input = TextEdit::new("");
            }
            McpTemplate::Playwright => {
                self.id_input = TextEdit::new("playwright");
                self.label_input = TextEdit::new("Playwright");
                self.transport_idx = 1;
                self.url_input = TextEdit::new("");
                self.auth_header_input = TextEdit::new("");
                self.command_input = TextEdit::new("npx");
                self.args_input = TextEdit::new("-y @playwright/mcp@latest");
                self.env_input = TextEdit::new("");
            }
            McpTemplate::TailscaleHttp => {
                self.id_input = TextEdit::new("tailscale-mcp");
                self.label_input = TextEdit::new("Tailscale HTTP");
                self.transport_idx = 0;
                self.url_input = TextEdit::new("http://100.x.y.z:3000/mcp");
                self.auth_header_input = TextEdit::new("");
                self.command_input = TextEdit::new("");
                self.args_input = TextEdit::new("");
                self.env_input = TextEdit::new("");
            }
        }
    }

    pub fn toggle_inspected_tool(&mut self, server_id: &str, tool: &McpUpstreamToolPreview) {
        let same = self
            .inspected_tool
            .as_ref()
            .is_some_and(|(sid, tname)| sid == server_id && tname == &tool.name);
        if same {
            self.inspected_tool = None;
            self.show_raw_schema = false;
            self.tool_args_error = None;
            return;
        }
        self.inspected_tool = Some((server_id.to_owned(), tool.name.clone()));
        self.show_raw_schema = false;
        self.tool_args_error = None;
        self.tool_args_input = TextEdit::new(schema_args_skeleton(tool.input_schema.as_ref()));
        if self
            .tool_call_output
            .as_ref()
            .is_some_and(|out| out.server_id != server_id || out.tool != tool.name)
        {
            self.tool_call_output = None;
        }
    }

    pub fn apply_tool_called(
        &mut self,
        id: String,
        tool: String,
        result: Result<McpToolCallResult, String>,
    ) {
        if self
            .tool_calling
            .as_ref()
            .is_some_and(|(sid, tname)| sid == &id && tname == &tool)
        {
            self.tool_calling = None;
        }
        self.tool_call_output = Some(ToolCallPanelResult {
            server_id: id,
            tool,
            outcome: result,
        });
    }

    pub fn open_new_form(&mut self) {
        self.form_open = true;
        self.editing_id = None;
        self.id_input = TextEdit::new("");
        self.label_input = TextEdit::new("");
        self.transport_idx = 0;
        self.url_input = TextEdit::new("");
        self.auth_header_input = TextEdit::new("");
        self.command_input = TextEdit::new("");
        self.args_input = TextEdit::new("");
        self.env_input = TextEdit::new("");
        self.scope_idx = 0;
        self.isolation_idx = 0;
        self.timeout_input = TextEdit::new("30");
        self.allowed_tools_input = TextEdit::new("");
        self.denied_tools_input = TextEdit::new("");
        self.form_saving = false;
        self.form_error = None;
        self.confirm_delete = None;
    }

    pub fn open_edit_form(&mut self, upstream: &McpUpstreamView) {
        self.form_open = true;
        self.editing_id = Some(upstream.id.clone());
        self.id_input = TextEdit::new(&upstream.id);
        self.label_input = TextEdit::new(upstream.label.as_deref().unwrap_or(""));
        match &upstream.transport {
            McpUpstreamTransport::Http { url, headers } => {
                self.transport_idx = 0;
                self.url_input = TextEdit::new(url);
                let auth = headers
                    .get("Authorization")
                    .or_else(|| headers.get("authorization"))
                    .cloned()
                    .unwrap_or_default();
                self.auth_header_input = TextEdit::new(auth);
                self.command_input = TextEdit::new("");
                self.args_input = TextEdit::new("");
                self.env_input = TextEdit::new("");
            }
            McpUpstreamTransport::Stdio { command, args, env } => {
                self.transport_idx = 1;
                self.url_input = TextEdit::new("");
                self.auth_header_input = TextEdit::new("");
                self.command_input = TextEdit::new(command);
                self.args_input = TextEdit::new(args.join(" "));
                let env_str = env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                self.env_input = TextEdit::new(env_str);
            }
        }
        self.scope_idx = scope_index(&upstream.scope);
        self.isolation_idx = isolation_index(&upstream.isolation);
        self.timeout_input = TextEdit::new(upstream.timeout_s.to_string());
        self.allowed_tools_input = TextEdit::new(upstream.allowed_tools.join(", "));
        self.denied_tools_input = TextEdit::new(upstream.denied_tools.join(", "));
        self.form_saving = false;
        self.form_error = None;
        self.confirm_delete = None;
    }

    pub fn close_form(&mut self) {
        self.form_open = false;
        self.editing_id = None;
        self.form_saving = false;
        self.form_error = None;
    }

    pub fn build_upsert(&self) -> Result<(String, McpUpstreamUpsert), String> {
        let id = self
            .editing_id
            .as_deref()
            .unwrap_or(self.id_input.text.as_str())
            .trim()
            .to_ascii_lowercase();
        if !valid_id(&id) {
            return Err(
                "ID must be 1–64 lowercase letters, digits, '-' or '_', and cannot be 'muqun'."
                    .into(),
            );
        }
        let label = {
            let trimmed = self.label_input.text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        };
        let timeout_s = {
            let raw = self.timeout_input.text.trim();
            if raw.is_empty() {
                None
            } else {
                let parsed = raw
                    .parse::<u64>()
                    .map_err(|_| "Timeout must be a number of seconds (1–120).".to_owned())?;
                if !(1..=120).contains(&parsed) {
                    return Err("Timeout must be between 1 and 120 seconds.".into());
                }
                Some(parsed)
            }
        };

        let enabled = self
            .editing_id
            .as_deref()
            .and_then(|eid| self.upstreams.iter().find(|u| u.id == eid))
            .is_none_or(|u| u.enabled);

        let transport = if self.transport_idx == 0 {
            let url = self.url_input.text.trim().to_owned();
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("HTTP URL must start with http:// or https://.".into());
            }
            let mut headers = BTreeMap::new();
            // Preserve non-Authorization headers when editing an existing HTTP upstream.
            if let Some(prev) = self
                .editing_id
                .as_deref()
                .and_then(|eid| self.upstreams.iter().find(|u| u.id == eid))
            {
                if let McpUpstreamTransport::Http {
                    headers: prev_headers,
                    ..
                } = &prev.transport
                {
                    for (k, v) in prev_headers {
                        if !k.eq_ignore_ascii_case("authorization") {
                            headers.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            let auth = self.auth_header_input.text.trim();
            if !auth.is_empty() {
                headers.insert("Authorization".into(), auth.to_owned());
            }
            McpUpstreamTransport::Http { url, headers }
        } else {
            let command = self.command_input.text.trim().to_owned();
            if command.is_empty() {
                return Err("Stdio command must not be empty.".into());
            }
            let args = split_args(&self.args_input.text);
            let env = parse_env(&self.env_input.text)?;
            McpUpstreamTransport::Stdio { command, args, env }
        };

        let allowed_tools = parse_patterns(&self.allowed_tools_input.text);
        let denied_tools = parse_patterns(&self.denied_tools_input.text);

        Ok((
            id,
            McpUpstreamUpsert {
                label,
                enabled,
                scope: SCOPES[self.scope_idx.min(SCOPES.len() - 1)].to_owned(),
                isolation: ISOLATIONS[self.isolation_idx.min(ISOLATIONS.len() - 1)].to_owned(),
                timeout_s,
                allowed_tools,
                denied_tools,
                transport,
            },
        ))
    }

    pub fn apply_loaded(&mut self, result: Result<McpHubSnapshot, McpHubError>) {
        self.loading = false;
        match result {
            Ok(snapshot) => {
                self.builtin = snapshot.builtin;
                self.upstreams = snapshot.upstreams;
                self.unsupported_reason = None;
                self.error = None;
            }
            Err(McpHubError::Unsupported(reason)) => {
                self.builtin = None;
                self.upstreams.clear();
                self.unsupported_reason = Some(reason);
                self.error = None;
            }
            Err(McpHubError::Failed(msg)) => {
                self.error = Some(msg);
            }
        }
    }

    pub fn apply_saved(&mut self, id: String, result: Result<McpUpstreamView, String>) {
        self.busy_ids.remove(&id);
        self.form_saving = false;
        match result {
            Ok(view) => {
                self.error = None;
                self.form_error = None;
                if let Some(slot) = self.upstreams.iter_mut().find(|u| u.id == view.id) {
                    *slot = view;
                } else {
                    self.upstreams.push(view);
                }
                if self.form_open
                    && (self.editing_id.as_deref() == Some(id.as_str())
                        || self.id_input.text.trim().eq_ignore_ascii_case(&id))
                {
                    self.close_form();
                }
            }
            Err(msg) => {
                if self.form_open {
                    self.form_error = Some(msg);
                } else {
                    self.error = Some(msg);
                }
            }
        }
    }

    pub fn apply_deleted(&mut self, id: String, result: Result<(), String>) {
        self.busy_ids.remove(&id);
        if self.confirm_delete.as_deref() == Some(id.as_str()) {
            self.confirm_delete = None;
        }
        match result {
            Ok(()) => {
                self.upstreams.retain(|u| u.id != id);
                if self
                    .inspected_tool
                    .as_ref()
                    .is_some_and(|(sid, _)| sid == &id)
                {
                    self.inspected_tool = None;
                    self.show_raw_schema = false;
                    self.tool_args_error = None;
                }
                if self
                    .tool_calling
                    .as_ref()
                    .is_some_and(|(sid, _)| sid == &id)
                {
                    self.tool_calling = None;
                }
                if self
                    .tool_call_output
                    .as_ref()
                    .is_some_and(|out| out.server_id == id)
                {
                    self.tool_call_output = None;
                }
                self.error = None;
            }
            Err(msg) => {
                self.error = Some(msg);
            }
        }
    }

    pub fn apply_probed(&mut self, id: String, result: Result<McpUpstreamView, String>) {
        self.busy_ids.remove(&id);
        match result {
            Ok(view) => {
                self.error = None;
                if let Some(slot) = self.upstreams.iter_mut().find(|u| u.id == view.id) {
                    *slot = view;
                } else {
                    self.upstreams.push(view);
                }
            }
            Err(msg) => {
                self.error = Some(msg);
            }
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id != "muqun"
        && (id.as_bytes()[0].is_ascii_lowercase() || id.as_bytes()[0].is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

fn scope_index(scope: &str) -> usize {
    SCOPES.iter().position(|s| *s == scope).unwrap_or(0)
}

fn isolation_index(iso: &str) -> usize {
    ISOLATIONS.iter().position(|s| *s == iso).unwrap_or(0)
}

fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        args.push(cur);
    }
    args
}

fn parse_env(input: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for raw in input.split([';', '\n']) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            return Err(format!(
                "Invalid env entry '{trimmed}': expected KEY=VALUE."
            ));
        };
        let key = k.trim();
        if key.is_empty() {
            return Err("Env key cannot be empty.".into());
        }
        map.insert(key.to_owned(), v.trim().to_owned());
    }
    Ok(map)
}

fn parse_patterns(input: &str) -> Vec<String> {
    input
        .split([',', ';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn upsert_from_view(view: &McpUpstreamView) -> McpUpstreamUpsert {
    McpUpstreamUpsert {
        label: view.label.clone(),
        enabled: view.enabled,
        scope: view.scope.clone(),
        isolation: view.isolation.clone(),
        timeout_s: Some(view.timeout_s),
        allowed_tools: view.allowed_tools.clone(),
        denied_tools: view.denied_tools.clone(),
        transport: view.transport.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaParam {
    name: String,
    ty: String,
    required: bool,
    description: Option<String>,
}

fn schema_type_label(prop: &serde_json::Value) -> String {
    if let Some(ty) = prop.get("type").and_then(serde_json::Value::as_str) {
        return ty.to_owned();
    }
    if let Some(arr) = prop.get("type").and_then(serde_json::Value::as_array) {
        let parts: Vec<&str> = arr.iter().filter_map(serde_json::Value::as_str).collect();
        if !parts.is_empty() {
            return parts.join(" | ");
        }
    }
    if prop.get("enum").is_some() {
        return "enum".to_owned();
    }
    if prop.get("anyOf").is_some() || prop.get("oneOf").is_some() {
        return "union".to_owned();
    }
    "any".to_owned()
}

fn extract_schema_params(schema: Option<&serde_json::Value>) -> Vec<SchemaParam> {
    let Some(props) = schema
        .and_then(|s| s.get("properties"))
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let required_set: HashSet<&str> = schema
        .and_then(|s| s.get("required"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();

    let mut params: Vec<SchemaParam> = props
        .iter()
        .map(|(name, prop)| SchemaParam {
            name: name.clone(),
            ty: schema_type_label(prop),
            required: required_set.contains(name.as_str()),
            description: prop
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        })
        .collect();
    params.sort_by(|a, b| {
        b.required
            .cmp(&a.required)
            .then_with(|| a.name.cmp(&b.name))
    });
    params
}

/// Build a compact single-line JSON skeleton from `input_schema.properties` so the
/// user can edit values immediately in the live tool tester.
pub fn schema_args_skeleton(schema: Option<&serde_json::Value>) -> String {
    let Some(props) = schema
        .and_then(|s| s.get("properties"))
        .and_then(serde_json::Value::as_object)
    else {
        return "{}".to_owned();
    };
    if props.is_empty() {
        return "{}".to_owned();
    }
    let required_set: HashSet<&str> = schema
        .and_then(|s| s.get("required"))
        .and_then(serde_json::Value::as_array)
        .map(|arr| arr.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();

    let mut ordered_keys: Vec<(&String, &serde_json::Value)> = props.iter().collect();
    ordered_keys.sort_by(|(ka, _), (kb, _)| {
        required_set
            .contains(kb.as_str())
            .cmp(&required_set.contains(ka.as_str()))
            .then_with(|| ka.cmp(kb))
    });

    // Prefer required keys when there are many optional properties; otherwise include all up to 8.
    let keys_to_include: Vec<(&String, &serde_json::Value)> =
        if !required_set.is_empty() && ordered_keys.len() > 6 {
            ordered_keys
                .into_iter()
                .filter(|(k, _)| required_set.contains(k.as_str()))
                .take(8)
                .collect()
        } else {
            ordered_keys.into_iter().take(8).collect()
        };

    let mut map = serde_json::Map::new();
    for (name, prop) in keys_to_include {
        let placeholder = if let Some(def) = prop.get("default") {
            def.clone()
        } else if let Some(first_enum) = prop
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .and_then(|a| a.first())
        {
            first_enum.clone()
        } else {
            let primary_ty = prop
                .get("type")
                .and_then(|t| {
                    t.as_str().or_else(|| {
                        t.as_array().and_then(|arr| {
                            arr.iter()
                                .filter_map(serde_json::Value::as_str)
                                .find(|s| *s != "null")
                        })
                    })
                })
                .unwrap_or("string");
            match primary_ty {
                "string" => serde_json::Value::String(String::new()),
                "integer" | "number" => serde_json::json!(0),
                "boolean" => serde_json::Value::Bool(false),
                "array" => serde_json::json!([]),
                "object" => serde_json::json!({}),
                _ => serde_json::Value::Null,
            }
        };
        map.insert(name.clone(), placeholder);
    }
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".to_owned())
}

fn format_json_preview(value: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let char_count = pretty.chars().count();
    if char_count <= MAX_PREVIEW_CHARS {
        pretty
    } else {
        let mut cut: String = pretty.chars().take(MAX_PREVIEW_CHARS).collect();
        cut.push_str("\n… (truncated)");
        cut
    }
}

fn parse_tool_arguments(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let val: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("Invalid JSON: {e}"))?;
    if !val.is_object() {
        return Err("Arguments must be a JSON object.".into());
    }
    Ok(val)
}

fn submit_tool_test(s: &mut UiState, server_id: &str, tool_name: &str) {
    let lang = s.cfg.general.language;
    match parse_tool_arguments(&s.mcp_hub.tool_args_input.text) {
        Ok(arguments) => {
            s.mcp_hub.tool_args_error = None;
            s.mcp_hub.tool_calling = Some((server_id.to_owned(), tool_name.to_owned()));
            s.send(Command::CallMcpUpstreamTool {
                id: server_id.to_owned(),
                tool: tool_name.to_owned(),
                arguments,
            });
        }
        Err(msg) => {
            let localized = if msg == "Arguments must be a JSON object." {
                tr_ref(lang, "Arguments must be a JSON object.").to_owned()
            } else {
                msg
            };
            s.mcp_hub.tool_args_error = Some(localized);
        }
    }
}

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
    if s.current_computer() == "all" {
        return Vec::new();
    }
    let mut out = vec![button(t("Refresh"))
        .disabled(s.mcp_hub.loading)
        .on_press(|s: &mut UiState| s.refresh_mcp_upstreams())
        .build(k)
        .key("mcp-hub-refresh")];
    if s.mcp_hub.unsupported_reason.is_none() {
        let label = if s.mcp_hub.form_open {
            t("Cancel")
        } else {
            t("Add MCP server")
        };
        out.push(
            button(label)
                .variant(if s.mcp_hub.form_open {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::Submit
                })
                .on_press(|s: &mut UiState| {
                    if s.mcp_hub.form_open {
                        s.mcp_hub.close_form();
                    } else {
                        s.mcp_hub.open_new_form();
                    }
                })
                .build(k)
                .key("mcp-hub-toggle-add"),
        );
    }
    out
}

fn target_host_card(s: &UiState, k: &Kit) -> El {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
    let comp = s.current_computer();

    if comp == "all" {
        let mut picker = hrow()
            .key("mcp-hub-host-picker")
            .gap(8.0)
            .flex_wrap()
            .center_y();
        picker = picker.child(
            button(t("This computer"))
                .variant(ButtonVariant::Outline)
                .on_press(|s: &mut UiState| s.switch_computer("local".into()))
                .build(k)
                .key("mcp-hub-pick-local"),
        );
        let mut remote_ids: Vec<(String, String)> = s
            .cfg
            .computers
            .keys()
            .filter(|id| UiState::is_remote_computer_id(id))
            .map(|id| (id.clone(), s.computer_name(id)))
            .collect();
        remote_ids.sort_by_key(|a| a.1.to_lowercase());
        for (id, name) in remote_ids {
            let target = id.clone();
            picker = picker.child(
                button(&name)
                    .variant(ButtonVariant::Outline)
                    .on_press(move |s: &mut UiState| s.switch_computer(target.clone()))
                    .build(k)
                    .key(format!("mcp-hub-pick-{id}")),
            );
        }
        return group_with(
            k,
            "mcp-hub-target-host",
            hrow()
                .key("target-head")
                .gap(8.0)
                .center_y()
                .child(
                    text(t("Select a computer"))
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base),
                ),
            vec![
                col()
                    .key("target-all-body")
                    .w_full()
                    .gap(10.0)
                    .child(
                        text(t(
                            "All computers view is active. Pick a specific computer to manage its MCP Hub.",
                        ))
                        .font(k.ty.small)
                        .color(k.c.text_muted),
                    )
                    .child(picker),
            ],
        );
    }

    let is_remote = UiState::is_remote_computer_id(comp);
    let host_name = s.computer_name(comp);
    let dot_color = if is_remote {
        s.computer_color(comp)
    } else {
        k.c.success.icon
    };
    let host_pill = hrow()
        .key("target-host-pill")
        .gap(6.0)
        .px(8.0)
        .py(2.0)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.5))
        .center_y()
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(dot_color)
                .a11y_hidden(),
        )
        .child(text(host_name).font(k.ty.small).color(k.c.text_base));

    let mut head = hrow()
        .key("target-head")
        .w_full()
        .gap(8.0)
        .center_y()
        .flex_wrap()
        .child(
            text(t("Target gateway"))
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .child(host_pill);
    if is_remote {
        head = head.child(pill(k, "target-remote-pill", t("Remote host")));
    }

    group_with(
        k,
        "mcp-hub-target-host",
        head,
        vec![text(t(
            "Changes apply to the Muqun Gateway running on this host.",
        ))
        .font(k.ty.small)
        .color(k.c.text_muted)],
    )
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
    let mut sections = vec![target_host_card(s, k)];

    if s.current_computer() == "all" {
        return sections;
    }

    if let Some(reason) = &s.mcp_hub.unsupported_reason {
        sections.push(empty(
            k,
            "mcp-hub-unsupported",
            t("Gateway update required"),
            reason,
        ));
        return sections;
    }

    if let Some(err) = &s.mcp_hub.error {
        sections.push(status(k, "mcp-hub-error", Tone::Bad, err.clone()));
    }

    if let Some(builtin) = &s.mcp_hub.builtin {
        sections.push(builtin_card(s, k, builtin));
    }

    if s.mcp_hub.form_open {
        sections.push(editor_form(s, k));
    }

    if s.mcp_hub.upstreams.is_empty() {
        if !s.mcp_hub.form_open {
            let detail = if s.mcp_hub.loading {
                t("Loading configured MCP servers…")
            } else {
                t("Configure external HTTP or stdio MCP servers once in Muqun Gateway. Every connected agent discovers them on demand via hub.search_tools and calls them via hub.call_tool without bloating tool schemas.")
            };
            sections.push(empty(
                k,
                "mcp-hub-empty",
                t("No external MCP servers yet"),
                detail,
            ));
        }
        return sections;
    }

    for upstream in &s.mcp_hub.upstreams {
        sections.push(upstream_card(s, k, upstream));
    }

    sections
}

fn builtin_card(s: &UiState, k: &Kit, builtin: &McpBuiltinView) -> El {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);

    let title = if builtin.label.is_empty() {
        format!("Muqun Gateway ({})", builtin.id)
    } else {
        format!("{} ({})", builtin.label, builtin.id)
    };

    let (dot_color, status_text) = if !builtin.enabled {
        (k.c.text_muted.alpha(0.5), t("Disabled").to_owned())
    } else if builtin.serves_agents {
        (
            k.c.success.icon,
            format!("{} · {} {}", t("Running"), builtin.tool_count, t("tools")),
        )
    } else {
        (k.c.danger.fg, t("Loopback unavailable").to_owned())
    };

    let status_pill = hrow()
        .key("builtin-status-pill")
        .gap(6.0)
        .px(8.0)
        .py(2.0)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.5))
        .center_y()
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(dot_color)
                .a11y_hidden(),
        )
        .child(text(status_text).font(k.ty.small).color(k.c.text_base));

    let inject_pill = pill(
        k,
        "builtin-inject-pill",
        if builtin.inject_into_agents {
            t("Auto-inject on")
        } else {
            t("Auto-inject off")
        },
    );

    let head = hrow()
        .key("head")
        .w_full()
        .min_h(32.0)
        .gap(10.0)
        .center_y()
        .child(
            hrow()
                .key("title-group")
                .gap(8.0)
                .center_y()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .flex_wrap()
                .child(
                    text(title)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(pill(k, "builtin-pill", t("Built-in")))
                .child(pill(k, "transport-pill", "HTTP"))
                .child(status_pill),
        )
        .child(inject_pill);

    let mut rows = Vec::new();

    let endpoint_detail = if builtin.protocol.is_empty() {
        builtin.url.clone()
    } else {
        format!("{}  ·  {}", builtin.url, builtin.protocol)
    };
    let token_badge = pill(
        k,
        "builtin-tokens-pill",
        format!("{}: {}", t("Active tokens"), builtin.active_tokens),
    );
    rows.push(row(
        k,
        "mcp-builtin-ep",
        t("Endpoint"),
        &endpoint_detail,
        token_badge,
    ));

    if !builtin.tools.is_empty() {
        rows.push(tools_section(
            s,
            k,
            &builtin.id,
            t("Built-in tools"),
            builtin.tool_count,
            &builtin.tools,
            false,
            false,
        ));
    }

    if !builtin.skills.is_empty() {
        let names = builtin
            .skills
            .iter()
            .map(|sk| format!("{}/{}", sk.namespace, sk.name))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(
            col()
                .key("mcp-builtin-skills")
                .w_full()
                .py(8.0)
                .gap(4.0)
                .child(
                    text(format!(
                        "{} ({})",
                        t("Available skills"),
                        builtin.skill_count
                    ))
                    .font(k.ty.small)
                    .color(k.c.text_base),
                )
                .child(
                    text(names)
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full(),
                ),
        );
    }

    group_with(k, "mcp-builtin-muqun", head, rows)
}

fn editor_form(s: &UiState, k: &Kit) -> El {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
    let editing = s.mcp_hub.editing_id.is_some();
    let title = if editing {
        t("Edit MCP server")
    } else {
        t("Add MCP server")
    };

    let mut rows = Vec::new();

    if !editing {
        let mut tpl_bar = hrow()
            .key("mcp-templates-bar")
            .gap(6.0)
            .flex_wrap()
            .center_y();
        for tpl in TEMPLATES {
            tpl_bar = tpl_bar.child(
                Button::new(tpl.button_label(lang))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.mcp_hub.apply_template(tpl);
                    })
                    .build(k)
                    .key(format!("mcp-tpl-{}", tpl.key())),
            );
        }
        rows.push(row(
            k,
            "mcp-templates",
            t("Quick templates"),
            t("Pre-fill common MCP servers with safe defaults."),
            tpl_bar,
        ));

        let id_in = TextInput::new(&s.mcp_hub.id_input)
            .label(t("Server ID"))
            .placeholder("github")
            .on_edit(|s: &mut UiState| &mut s.mcp_hub.id_input);
        rows.push(row(
            k,
            "mcp-id",
            t("Server ID"),
            t("Lowercase identifier used by hub.call_tool (e.g. github, notion)."),
            field(id_in, &s.mcp_hub.id_input, k, "mcp-id"),
        ));
    }

    let label_in = TextInput::new(&s.mcp_hub.label_input)
        .label(t("Display name"))
        .placeholder("GitHub")
        .on_edit(|s: &mut UiState| &mut s.mcp_hub.label_input);
    rows.push(row(
        k,
        "mcp-label",
        t("Display name"),
        t("Optional friendly name shown in Settings."),
        field(label_in, &s.mcp_hub.label_input, k, "mcp-label"),
    ));

    let transport_labels = [t("Streamable HTTP"), t("Stdio command")];
    rows.push(row(
        k,
        "mcp-transport",
        t("Transport"),
        t("Connect to a remote/local HTTP MCP endpoint or spawn a local stdio MCP process."),
        choice(
            k,
            "mcp-transport",
            &transport_labels,
            s.mcp_hub.transport_idx,
            |s, i| {
                s.mcp_hub.transport_idx = i;
                s.mcp_hub.form_error = None;
            },
        ),
    ));

    if s.mcp_hub.transport_idx == 0 {
        let url_in = TextInput::new(&s.mcp_hub.url_input)
            .label(t("Endpoint URL"))
            .placeholder("https://api.example.com/mcp")
            .on_edit(|s: &mut UiState| &mut s.mcp_hub.url_input);
        rows.push(row(
            k,
            "mcp-url",
            t("Endpoint URL"),
            t("Streamable HTTP MCP endpoint URL."),
            field(url_in, &s.mcp_hub.url_input, k, "mcp-url"),
        ));

        let auth_in = TextInput::new(&s.mcp_hub.auth_header_input)
            .label(t("Authorization header"))
            .placeholder("Bearer ...")
            .on_edit(|s: &mut UiState| &mut s.mcp_hub.auth_header_input);
        rows.push(row(
            k,
            "mcp-auth",
            t("Authorization header"),
            t("Optional Authorization header (masked as •••••••• after saving; leave unchanged to keep existing secret)."),
            field(auth_in, &s.mcp_hub.auth_header_input, k, "mcp-auth"),
        ));
    } else {
        let cmd_in = TextInput::new(&s.mcp_hub.command_input)
            .label(t("Command"))
            .placeholder("npx")
            .on_edit(|s: &mut UiState| &mut s.mcp_hub.command_input);
        rows.push(row(
            k,
            "mcp-command",
            t("Command"),
            t("Executable to spawn for this stdio MCP server."),
            field(cmd_in, &s.mcp_hub.command_input, k, "mcp-command"),
        ));

        let args_in = TextInput::new(&s.mcp_hub.args_input)
            .label(t("Arguments"))
            .placeholder("-y @modelcontextprotocol/server-filesystem /path")
            .on_edit(|s: &mut UiState| &mut s.mcp_hub.args_input);
        rows.push(row(
            k,
            "mcp-args",
            t("Arguments"),
            t("Space-separated command arguments (supports quotes)."),
            field(args_in, &s.mcp_hub.args_input, k, "mcp-args"),
        ));

        let env_in = TextInput::new(&s.mcp_hub.env_input)
            .label(t("Environment variables"))
            .placeholder("API_KEY=...")
            .on_edit(|s: &mut UiState| &mut s.mcp_hub.env_input);
        rows.push(row(
            k,
            "mcp-env",
            t("Environment variables"),
            t("Optional KEY=VALUE pairs separated by ';' (sensitive values stay masked on edit)."),
            field(env_in, &s.mcp_hub.env_input, k, "mcp-env"),
        ));
    }

    let scope_labels = [t("Local only"), t("Local + tasks"), t("All")];
    rows.push(row(
        k,
        "mcp-scope",
        t("Access scope"),
        t("Default Local only blocks delegated sub-tasks and remote peer tasks from calling this server."),
        choice(k, "mcp-scope", &scope_labels, s.mcp_hub.scope_idx, |s, i| {
            s.mcp_hub.scope_idx = i;
        }),
    ));

    let iso_labels = [t("Shared"), t("Per session")];
    rows.push(row(
        k,
        "mcp-isolation",
        t("Isolation"),
        t("Shared reuses one connection/process; Per session starts a fresh one per call."),
        choice(
            k,
            "mcp-isolation",
            &iso_labels,
            s.mcp_hub.isolation_idx,
            |s, i| {
                s.mcp_hub.isolation_idx = i;
            },
        ),
    ));

    let timeout_in = TextInput::new(&s.mcp_hub.timeout_input)
        .label(t("Timeout (seconds)"))
        .placeholder("30")
        .on_edit(|s: &mut UiState| &mut s.mcp_hub.timeout_input);
    rows.push(row(
        k,
        "mcp-timeout",
        t("Timeout (seconds)"),
        t("Per-call timeout (1–120 seconds, default 30)."),
        field(timeout_in, &s.mcp_hub.timeout_input, k, "mcp-timeout"),
    ));

    let allow_in = TextInput::new(&s.mcp_hub.allowed_tools_input)
        .label(t("Allowed tools"))
        .placeholder("read_*, list_*, get_*")
        .on_edit(|s: &mut UiState| &mut s.mcp_hub.allowed_tools_input);
    rows.push(row(
        k,
        "mcp-allow-tools",
        t("Allowed tools"),
        t("Optional comma-separated allowlist (supports * wildcards). Empty allows all non-blocked tools."),
        field(allow_in, &s.mcp_hub.allowed_tools_input, k, "mcp-allow-tools"),
    ));

    let deny_in = TextInput::new(&s.mcp_hub.denied_tools_input)
        .label(t("Blocked tools"))
        .placeholder("delete_*, drop_*, exec_*")
        .on_edit(|s: &mut UiState| &mut s.mcp_hub.denied_tools_input);
    rows.push(row(
        k,
        "mcp-deny-tools",
        t("Blocked tools"),
        t("Optional comma-separated denylist (supports * wildcards). Always overrides allowed tools."),
        field(deny_in, &s.mcp_hub.denied_tools_input, k, "mcp-deny-tools"),
    ));

    if let Some(err) = &s.mcp_hub.form_error {
        rows.push(status(k, "mcp-form-err", Tone::Bad, err.clone()));
    }

    let form_actions = hrow()
        .key("mcp-form-actions")
        .gap(8.0)
        .center_y()
        .child(
            button(t("Cancel"))
                .disabled(s.mcp_hub.form_saving)
                .on_press(|s: &mut UiState| s.mcp_hub.close_form())
                .build(k)
                .key("mcp-form-cancel"),
        )
        .child(
            button(if s.mcp_hub.form_saving {
                t("Saving…")
            } else {
                t("Save")
            })
            .variant(ButtonVariant::Submit)
            .disabled(s.mcp_hub.form_saving)
            .on_press(|s: &mut UiState| match s.mcp_hub.build_upsert() {
                Ok((id, body)) => {
                    s.mcp_hub.form_error = None;
                    s.mcp_hub.form_saving = true;
                    s.mcp_hub.busy_ids.insert(id.clone());
                    s.send(Command::UpsertMcpUpstream { id, body });
                }
                Err(msg) => {
                    s.mcp_hub.form_error = Some(msg);
                }
            })
            .build(k)
            .key("mcp-form-save"),
        );

    rows.push(row(k, "mcp-form-submit", "", "", form_actions));
    group(k, "mcp-hub-form", title, rows)
}

fn pill(k: &Kit, key: &str, label: impl Into<String>) -> El {
    hrow()
        .key(key.to_string())
        .px(8.0)
        .py(2.0)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.5))
        .center_y()
        .child(text(label.into()).font(k.ty.small).color(k.c.text_muted))
}

fn upstream_card(s: &UiState, k: &Kit, upstream: &McpUpstreamView) -> El {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
    let id = upstream.id.clone();
    let busy = s.mcp_hub.busy_ids.contains(&id);

    let title = upstream
        .label
        .as_deref()
        .filter(|l| !l.is_empty())
        .map(|l| format!("{l} ({})", upstream.id))
        .unwrap_or_else(|| upstream.id.clone());

    let transport_tag = match &upstream.transport {
        McpUpstreamTransport::Http { .. } => "HTTP",
        McpUpstreamTransport::Stdio { .. } => "stdio",
    };

    let (dot_color, status_text) = if !upstream.enabled {
        (k.c.text_muted.alpha(0.5), t("Disabled").to_owned())
    } else if upstream.healthy {
        (
            k.c.success.icon,
            format!("{} {}", upstream.tool_count, t("tools")),
        )
    } else if upstream.last_error.is_some() {
        (k.c.danger.fg, t("Error").to_owned())
    } else {
        (k.c.text_muted.alpha(0.6), t("Not probed").to_owned())
    };

    let status_pill = hrow()
        .key("status-pill")
        .gap(6.0)
        .px(8.0)
        .py(2.0)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.5))
        .center_y()
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(dot_color)
                .a11y_hidden(),
        )
        .child(text(status_text).font(k.ty.small).color(k.c.text_base));

    let toggle_view = upstream.clone();
    let toggle_switch = flag(
        k,
        &format!("mcp-enable-{}", upstream.id),
        upstream.enabled,
        &title,
        move |s: &mut UiState| {
            let mut body = upsert_from_view(&toggle_view);
            body.enabled = !toggle_view.enabled;
            let uid = toggle_view.id.clone();
            s.mcp_hub.busy_ids.insert(uid.clone());
            s.send(Command::UpsertMcpUpstream { id: uid, body });
        },
    );

    let head = hrow()
        .key("head")
        .w_full()
        .min_h(32.0)
        .gap(10.0)
        .center_y()
        .child(
            hrow()
                .key("title-group")
                .gap(8.0)
                .center_y()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .flex_wrap()
                .child(
                    text(title)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(pill(k, "transport-pill", transport_tag))
                .child(status_pill),
        )
        .child(toggle_switch);

    let mut rows = Vec::new();

    let endpoint_summary = match &upstream.transport {
        McpUpstreamTransport::Http { url, headers } => {
            if headers.is_empty() {
                url.clone()
            } else {
                let keys = headers.keys().cloned().collect::<Vec<_>>().join(", ");
                format!("{url}  [{keys}]")
            }
        }
        McpUpstreamTransport::Stdio { command, args, .. } => {
            if args.is_empty() {
                command.clone()
            } else {
                format!("{command} {}", args.join(" "))
            }
        }
    };

    let probe_id = id.clone();
    let edit_view = upstream.clone();
    let del_id = id.clone();
    let confirming = s.mcp_hub.confirm_delete.as_deref() == Some(id.as_str());

    let mut card_actions = hrow()
        .key(format!("mcp-actions-{id}"))
        .gap(8.0)
        .center_y()
        .flex_wrap()
        .child(
            button(if busy { t("Working…") } else { t("Probe") })
                .disabled(busy)
                .on_press(move |s: &mut UiState| {
                    s.mcp_hub.busy_ids.insert(probe_id.clone());
                    s.send(Command::ProbeMcpUpstream {
                        id: probe_id.clone(),
                    });
                })
                .build(k)
                .key(format!("mcp-probe-{id}")),
        )
        .child(
            button(t("Edit"))
                .disabled(busy)
                .on_press(move |s: &mut UiState| {
                    s.mcp_hub.open_edit_form(&edit_view);
                })
                .build(k)
                .key(format!("mcp-edit-{id}")),
        );

    if confirming {
        let cancel_id = id.clone();
        card_actions = card_actions
            .child(
                button(t("Confirm delete"))
                    .variant(ButtonVariant::Danger)
                    .disabled(busy)
                    .on_press(move |s: &mut UiState| {
                        s.mcp_hub.busy_ids.insert(del_id.clone());
                        s.send(Command::DeleteMcpUpstream { id: del_id.clone() });
                    })
                    .build(k)
                    .key(format!("mcp-del-confirm-{id}")),
            )
            .child(
                button(t("Cancel"))
                    .on_press(move |s: &mut UiState| {
                        if s.mcp_hub.confirm_delete.as_deref() == Some(cancel_id.as_str()) {
                            s.mcp_hub.confirm_delete = None;
                        }
                    })
                    .build(k)
                    .key(format!("mcp-del-cancel-{id}")),
            );
    } else {
        card_actions = card_actions.child(
            button(t("Delete"))
                .disabled(busy)
                .on_press(move |s: &mut UiState| {
                    s.mcp_hub.confirm_delete = Some(del_id.clone());
                })
                .build(k)
                .key(format!("mcp-del-{id}")),
        );
    }

    rows.push(row(
        k,
        &format!("mcp-ep-{id}"),
        t("Endpoint"),
        &endpoint_summary,
        card_actions,
    ));

    let scope_labels = [t("Local only"), t("Local + tasks"), t("All")];
    let scope_view = upstream.clone();
    rows.push(row(
        k,
        &format!("mcp-scope-{id}"),
        t("Access scope"),
        t("Controls which Muqun sessions and tasks may call this MCP server."),
        choice(
            k,
            &format!("mcp-scope-{id}"),
            &scope_labels,
            scope_index(&upstream.scope),
            move |s, i| {
                let mut body = upsert_from_view(&scope_view);
                body.scope = SCOPES[i.min(SCOPES.len() - 1)].to_owned();
                let uid = scope_view.id.clone();
                s.mcp_hub.busy_ids.insert(uid.clone());
                s.send(Command::UpsertMcpUpstream { id: uid, body });
            },
        ),
    ));

    if let Some(err) = &upstream.last_error {
        rows.push(status(k, &format!("mcp-err-{id}"), Tone::Bad, err.clone()));
    }

    if !upstream.tools.is_empty() {
        rows.push(tools_section(
            s,
            k,
            &upstream.id,
            t("Discovered tools"),
            upstream.tool_count,
            &upstream.tools,
            true,
            upstream.enabled,
        ));
    }

    group_with(k, &format!("mcp-upstream-{id}"), head, rows)
}

#[allow(clippy::too_many_arguments)]
fn tools_section(
    s: &UiState,
    k: &Kit,
    server_id: &str,
    heading: &str,
    total_count: usize,
    tools: &[McpUpstreamToolPreview],
    is_external: bool,
    server_enabled: bool,
) -> El {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);

    let inspected_name = s
        .mcp_hub
        .inspected_tool
        .as_ref()
        .filter(|(sid, _)| sid == server_id)
        .map(|(_, tname)| tname.as_str());

    let hint = if is_external {
        t("Click a tool to inspect its schema or run a live test")
    } else {
        t("Click a tool to inspect its schema")
    };

    let mut chips = hrow()
        .key(format!("mcp-tool-chips-{server_id}"))
        .w_full()
        .gap(6.0)
        .flex_wrap()
        .center_y();

    for tool in tools {
        let selected = inspected_name == Some(tool.name.as_str());
        let sid = server_id.to_owned();
        let tool_clone = tool.clone();
        chips = chips.child(
            Button::new(&tool.name)
                .variant(if selected {
                    ButtonVariant::Contrast
                } else {
                    ButtonVariant::Outline
                })
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    s.mcp_hub.toggle_inspected_tool(&sid, &tool_clone);
                })
                .build(k)
                .key(format!("mcp-tool-chip-{server_id}-{}", tool.name)),
        );
    }

    if total_count > tools.len() {
        chips = chips.child(pill(
            k,
            &format!("mcp-tools-more-{server_id}"),
            format!("+{}", total_count - tools.len()),
        ));
    }

    let mut section = col()
        .key(format!("mcp-tools-{server_id}"))
        .w_full()
        .py(8.0)
        .gap(8.0)
        .child(
            hrow()
                .key(format!("mcp-tools-head-{server_id}"))
                .w_full()
                .gap(8.0)
                .center_y()
                .flex_wrap()
                .child(
                    text(format!("{heading} ({total_count})"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_base),
                )
                .child(text(hint).font(k.ty.small).color(k.c.text_muted)),
        )
        .child(chips);

    if let Some(inspected_tool) =
        inspected_name.and_then(|name| tools.iter().find(|t| t.name == name))
    {
        section = section.child(tool_inspector_panel(
            s,
            k,
            server_id,
            inspected_tool,
            is_external,
            server_enabled,
        ));
    }

    section
}

fn tool_inspector_panel(
    s: &UiState,
    k: &Kit,
    server_id: &str,
    tool: &McpUpstreamToolPreview,
    is_external: bool,
    server_enabled: bool,
) -> El {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);

    let mut header_actions = hrow()
        .key(format!("mcp-insp-actions-{server_id}-{}", tool.name))
        .gap(6.0)
        .center_y();
    if tool.input_schema.is_some() {
        let toggle_label = if s.mcp_hub.show_raw_schema {
            t("Hide JSON")
        } else {
            t("Raw JSON")
        };
        header_actions = header_actions.child(
            Button::new(toggle_label)
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    s.mcp_hub.show_raw_schema = !s.mcp_hub.show_raw_schema;
                })
                .build(k)
                .key(format!("mcp-insp-raw-{server_id}-{}", tool.name)),
        );
    }
    header_actions = header_actions.child(
        Button::new(t("Close"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(|s: &mut UiState| {
                s.mcp_hub.inspected_tool = None;
                s.mcp_hub.show_raw_schema = false;
                s.mcp_hub.tool_args_error = None;
            })
            .build(k)
            .key(format!("mcp-insp-close-{server_id}-{}", tool.name)),
    );

    let header = hrow()
        .key(format!("mcp-insp-head-{server_id}-{}", tool.name))
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            text(tool.name.clone())
                .font(k.ty.mono)
                .color(k.c.text_base)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .truncate(),
        )
        .child(header_actions);

    let desc_text = if tool.description.trim().is_empty() {
        t("No description provided.").to_owned()
    } else {
        tool.description.clone()
    };

    let mut panel = col()
        .key(format!("mcp-inspector-{server_id}-{}", tool.name))
        .w_full()
        .px(12.0)
        .py(12.0)
        .gap(10.0)
        .radius(k.radius.md)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.6))
        .child(header)
        .child(
            text(desc_text)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
        );

    let params = extract_schema_params(tool.input_schema.as_ref());
    if !params.is_empty() {
        let mut list = col()
            .key(format!("mcp-insp-params-{server_id}-{}", tool.name))
            .w_full()
            .gap(6.0)
            .child(
                text(t("Parameters"))
                    .font(k.ty.small_medium)
                    .color(k.c.text_base),
            );
        for param in &params {
            let mut line = hrow()
                .key(format!(
                    "mcp-param-{server_id}-{}-{}",
                    tool.name, param.name
                ))
                .w_full()
                .gap(6.0)
                .center_y()
                .flex_wrap()
                .child(
                    text(param.name.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_base),
                )
                .child(pill(
                    k,
                    &format!("mcp-param-ty-{server_id}-{}-{}", tool.name, param.name),
                    param.ty.clone(),
                ));
            if param.required {
                line = line.child(
                    hrow()
                        .key(format!(
                            "mcp-param-req-{server_id}-{}-{}",
                            tool.name, param.name
                        ))
                        .px(6.0)
                        .py(1.0)
                        .radius(k.radius.sm)
                        .fill(k.c.layer_01)
                        .border(1.0, k.c.border_weak.alpha(0.6))
                        .center_y()
                        .child(text(t("required")).font(k.ty.small).color(k.c.text_accent)),
                );
            }
            let mut item = col()
                .key(format!(
                    "mcp-param-item-{server_id}-{}-{}",
                    tool.name, param.name
                ))
                .w_full()
                .gap(2.0)
                .child(line);
            if let Some(desc) = &param.description {
                item = item.child(
                    text(desc.clone())
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full(),
                );
            }
            list = list.child(item);
        }
        panel = panel.child(list);
    } else if tool.input_schema.is_some() {
        panel = panel.child(
            text(t("No parameters required."))
                .font(k.ty.small)
                .color(k.c.text_muted),
        );
    } else {
        panel = panel.child(
            text(t("Input schema unavailable."))
                .font(k.ty.small)
                .color(k.c.text_muted),
        );
    }

    if s.mcp_hub.show_raw_schema {
        if let Some(schema) = &tool.input_schema {
            panel = panel.child(
                col()
                    .key(format!("mcp-insp-rawbox-{server_id}-{}", tool.name))
                    .w_full()
                    .px(8.0)
                    .py(8.0)
                    .radius(k.radius.sm)
                    .fill(k.c.bg_base)
                    .border(1.0, k.c.border_weak.alpha(0.45))
                    .child(
                        text(format_json_preview(schema))
                            .font(k.ty.mono)
                            .color(k.c.text_muted)
                            .wrap()
                            .w_full(),
                    ),
            );
        }
    }

    if is_external && server_id != "muqun" {
        panel = panel.child(
            el().key(format!("mcp-insp-rule-{server_id}-{}", tool.name))
                .w_full()
                .h(1.0)
                .fill(k.c.border_weak.alpha(0.35))
                .a11y_hidden(),
        );
        if !server_enabled {
            panel = panel.child(
                text(t("Enable this server to test tools live."))
                    .font(k.ty.small)
                    .color(k.c.text_muted),
            );
        } else {
            let calling = s
                .mcp_hub
                .tool_calling
                .as_ref()
                .is_some_and(|(sid, tname)| sid == server_id && tname == &tool.name);
            let run_sid = server_id.to_owned();
            let run_tool = tool.name.clone();
            let submit_sid = server_id.to_owned();
            let submit_tool = tool.name.clone();

            let args_field = TextInput::new(&s.mcp_hub.tool_args_input)
                .label(t("Test arguments (JSON)"))
                .placeholder("{}")
                .width(None)
                .on_edit(|s: &mut UiState| &mut s.mcp_hub.tool_args_input)
                .on_submit(move |s: &mut UiState, _: &mut rune::Tx| {
                    submit_tool_test(s, &submit_sid, &submit_tool);
                })
                .build(k)
                .key(format!("mcp-test-args-{server_id}-{}", tool.name))
                .grow(1.0)
                .shrink()
                .min_w(0.0);

            let run_btn = Button::new(if calling {
                t("Running…")
            } else {
                t("Test tool")
            })
            .variant(ButtonVariant::Submit)
            .control_size(ControlSize::Regular)
            .disabled(calling)
            .on_press(move |s: &mut UiState| {
                submit_tool_test(s, &run_sid, &run_tool);
            })
            .build(k)
            .key(format!("mcp-test-run-{server_id}-{}", tool.name));

            let mut tester = col()
                .key(format!("mcp-tester-{server_id}-{}", tool.name))
                .w_full()
                .gap(8.0)
                .child(
                    text(t("Test arguments (JSON)"))
                        .font(k.ty.small_medium)
                        .color(k.c.text_base),
                )
                .child(
                    hrow()
                        .key(format!("mcp-tester-row-{server_id}-{}", tool.name))
                        .w_full()
                        .gap(8.0)
                        .center_y()
                        .child(args_field)
                        .child(run_btn),
                );

            if let Some(arg_err) = &s.mcp_hub.tool_args_error {
                tester = tester.child(status(
                    k,
                    &format!("mcp-test-arg-err-{server_id}-{}", tool.name),
                    Tone::Bad,
                    arg_err.clone(),
                ));
            }

            if let Some(output) = s
                .mcp_hub
                .tool_call_output
                .as_ref()
                .filter(|out| out.server_id == server_id && out.tool == tool.name)
            {
                match &output.outcome {
                    Ok(res) => {
                        let mut meta = hrow()
                            .key(format!("mcp-test-meta-{server_id}-{}", tool.name))
                            .w_full()
                            .gap(8.0)
                            .center_y()
                            .child(status(
                                k,
                                &format!("mcp-test-latency-{server_id}-{}", tool.name),
                                Tone::Good,
                                format!("{} ms", res.duration_ms),
                            ));
                        if res.redacted {
                            meta = meta.child(pill(
                                k,
                                &format!("mcp-test-redacted-{server_id}-{}", tool.name),
                                t("Redacted"),
                            ));
                        }
                        tester = tester.child(meta).child(
                            col()
                                .key(format!("mcp-test-result-{server_id}-{}", tool.name))
                                .w_full()
                                .px(8.0)
                                .py(8.0)
                                .radius(k.radius.sm)
                                .fill(k.c.bg_base)
                                .border(1.0, k.c.border_weak.alpha(0.45))
                                .child(
                                    text(format_json_preview(&res.result))
                                        .font(k.ty.mono)
                                        .color(k.c.text_base)
                                        .wrap()
                                        .w_full(),
                                ),
                        );
                    }
                    Err(err) => {
                        tester = tester.child(status(
                            k,
                            &format!("mcp-test-err-{server_id}-{}", tool.name),
                            Tone::Bad,
                            err.clone(),
                        ));
                    }
                }
            }

            panel = panel.child(tester);
        }
    }

    panel
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::McpUpstreamToolPreview;

    #[test]
    fn build_upsert_validates_and_preserves_masked_secrets() {
        let mut st = McpHubState::default();
        st.open_new_form();
        st.id_input = TextEdit::new("muqun");
        st.url_input = TextEdit::new("https://example.com/mcp");
        assert!(st.build_upsert().is_err(), "'muqun' is reserved");

        st.id_input = TextEdit::new("GitHub_Prod");
        st.label_input = TextEdit::new("GitHub");
        st.auth_header_input = TextEdit::new("Bearer tok-123");
        st.scope_idx = 1;
        st.isolation_idx = 1;
        let (id, body) = st.build_upsert().unwrap();
        assert_eq!(id, "github_prod");
        assert_eq!(body.scope, "tasks");
        assert_eq!(body.isolation, "per_session");
        match body.transport {
            McpUpstreamTransport::Http { url, headers } => {
                assert_eq!(url, "https://example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer tok-123")
                );
            }
            _ => panic!("expected http"),
        }

        // Editing an existing server keeps MASKED_SECRET and non-auth headers intact.
        let mut existing_headers = BTreeMap::new();
        existing_headers.insert("Authorization".into(), MASKED_SECRET.into());
        existing_headers.insert("X-Tenant".into(), "acme".into());
        let view = McpUpstreamView {
            id: "github_prod".into(),
            label: Some("GitHub".into()),
            enabled: true,
            scope: "local_only".into(),
            isolation: "shared".into(),
            timeout_s: 30,
            allowed_tools: Vec::new(),
            denied_tools: vec!["delete_*".into()],
            transport: McpUpstreamTransport::Http {
                url: "https://example.com/mcp".into(),
                headers: existing_headers,
            },
            healthy: true,
            tool_count: 1,
            skill_count: 0,
            tools: vec![McpUpstreamToolPreview {
                name: "create_issue".into(),
                description: "Create issue".into(),
                input_schema: None,
            }],
            last_probed_ms: Some(1234),
            last_error: None,
        };
        st.upstreams = vec![view.clone()];
        st.open_edit_form(&view);
        assert_eq!(st.auth_header_input.text, MASKED_SECRET);
        let (_, edited) = st.build_upsert().unwrap();
        match edited.transport {
            McpUpstreamTransport::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some(MASKED_SECRET)
                );
                assert_eq!(headers.get("X-Tenant").map(String::as_str), Some("acme"));
            }
            _ => panic!("expected http"),
        }
    }

    #[test]
    fn stdio_form_splits_quoted_args_and_env() {
        let mut st = McpHubState::default();
        st.open_new_form();
        st.id_input = TextEdit::new("fs");
        st.transport_idx = 1;
        st.command_input = TextEdit::new("npx");
        st.args_input = TextEdit::new("-y @mcp/fs \"/my folder\"");
        st.env_input = TextEdit::new("NODE_ENV=production; API_KEY=abc");
        let (id, body) = st.build_upsert().unwrap();
        assert_eq!(id, "fs");
        match body.transport {
            McpUpstreamTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@mcp/fs", "/my folder"]);
                assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("production"));
                assert_eq!(env.get("API_KEY").map(String::as_str), Some("abc"));
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn state_transitions_update_and_clear_correctly() {
        let mut st = McpHubState::default();
        st.apply_loaded(Err(McpHubError::Unsupported("upgrade gateway".into())));
        assert_eq!(st.unsupported_reason.as_deref(), Some("upgrade gateway"));

        st.apply_loaded(Ok(McpHubSnapshot {
            builtin: Some(McpBuiltinView {
                id: "muqun".into(),
                label: "Muqun Gateway".into(),
                enabled: true,
                serves_agents: true,
                inject_into_agents: true,
                url: "http://127.0.0.1:23847/mcp".into(),
                protocol: "Streamable HTTP (MCP 2026-07-28)".into(),
                active_tokens: 2,
                tool_count: 12,
                skill_count: 4,
                tools: Vec::new(),
                skills: Vec::new(),
            }),
            upstreams: Vec::new(),
        }));
        assert!(st.unsupported_reason.is_none());
        assert_eq!(st.builtin.as_ref().map(|b| b.id.as_str()), Some("muqun"));

        let view = McpUpstreamView {
            id: "demo".into(),
            label: None,
            enabled: true,
            scope: "local_only".into(),
            isolation: "shared".into(),
            timeout_s: 30,
            allowed_tools: Vec::new(),
            denied_tools: vec!["delete_*".into()],
            transport: McpUpstreamTransport::Http {
                url: "http://127.0.0.1:8080/mcp".into(),
                headers: BTreeMap::new(),
            },
            healthy: true,
            tool_count: 2,
            skill_count: 0,
            tools: Vec::new(),
            last_probed_ms: Some(99),
            last_error: None,
        };
        st.open_new_form();
        st.id_input = TextEdit::new("demo");
        st.apply_saved("demo".into(), Ok(view.clone()));
        assert!(!st.form_open);
        assert_eq!(st.upstreams.len(), 1);

        st.apply_deleted("demo".into(), Ok(()));
        assert!(st.upstreams.is_empty());
    }

    #[test]
    fn quick_templates_prefill_safe_defaults_for_stdio_and_http() {
        let mut st = McpHubState::default();
        st.open_new_form();

        st.apply_template(McpTemplate::Filesystem);
        assert_eq!(st.id_input.text, "filesystem");
        assert_eq!(st.transport_idx, 1);
        assert_eq!(st.command_input.text, "npx");
        assert!(st
            .args_input
            .text
            .contains("@modelcontextprotocol/server-filesystem"));
        assert_eq!(st.scope_idx, 0);
        assert_eq!(st.timeout_input.text, "30");

        st.apply_template(McpTemplate::GitHub);
        assert_eq!(st.id_input.text, "github");
        assert_eq!(st.transport_idx, 1);
        assert_eq!(st.env_input.text, "GITHUB_PERSONAL_ACCESS_TOKEN=");

        st.apply_template(McpTemplate::Context7);
        assert_eq!(st.id_input.text, "context7");
        assert!(st.args_input.text.contains("@upstash/context7-mcp"));

        st.apply_template(McpTemplate::Playwright);
        assert_eq!(st.id_input.text, "playwright");
        assert!(st.args_input.text.contains("@playwright/mcp@latest"));

        st.apply_template(McpTemplate::TailscaleHttp);
        assert_eq!(st.id_input.text, "tailscale-mcp");
        assert_eq!(st.transport_idx, 0);
        assert_eq!(st.url_input.text, "http://100.x.y.z:3000/mcp");
        assert_eq!(st.scope_idx, 0);
        assert!(st.command_input.text.is_empty());
        let (id, body) = st.build_upsert().unwrap();
        assert_eq!(id, "tailscale-mcp");
        assert_eq!(body.scope, "local_only");
    }

    #[test]
    fn schema_skeleton_generates_typed_json_and_fallback() {
        assert_eq!(schema_args_skeleton(None), "{}");
        assert_eq!(
            schema_args_skeleton(Some(&serde_json::json!({ "type": "object" }))),
            "{}"
        );

        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Issue title" },
                "limit": { "type": "integer" },
                "draft": { "type": "boolean" },
                "labels": { "type": "array" },
                "mode": { "type": "string", "enum": ["fast", "deep"] }
            },
            "required": ["title"]
        });
        let skeleton = schema_args_skeleton(Some(&schema));
        let parsed: serde_json::Value = serde_json::from_str(&skeleton).unwrap();
        assert_eq!(parsed["title"], "");
        assert_eq!(parsed["limit"], 0);
        assert_eq!(parsed["draft"], false);
        assert_eq!(parsed["labels"], serde_json::json!([]));
        assert_eq!(parsed["mode"], "fast");

        let params = extract_schema_params(Some(&schema));
        assert_eq!(params[0].name, "title");
        assert!(params[0].required);
        assert_eq!(params[0].ty, "string");
    }

    #[test]
    fn toggle_inspected_tool_and_apply_tool_called_update_state() {
        let mut st = McpHubState::default();
        let tool = McpUpstreamToolPreview {
            name: "create_issue".into(),
            description: "Create a GitHub issue".into(),
            input_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"]
            })),
        };

        st.toggle_inspected_tool("github", &tool);
        assert_eq!(
            st.inspected_tool,
            Some(("github".into(), "create_issue".into()))
        );
        assert_eq!(st.tool_args_input.text, r#"{"title":""}"#);

        st.tool_calling = Some(("github".into(), "create_issue".into()));
        let call_res = McpToolCallResult {
            upstream: "github".into(),
            tool: "create_issue".into(),
            result: serde_json::json!({ "content": [{ "type": "text", "text": "ok" }] }),
            redacted: true,
            duration_ms: 42,
        };
        st.apply_tool_called("github".into(), "create_issue".into(), Ok(call_res.clone()));
        assert!(st.tool_calling.is_none());
        let output = st.tool_call_output.as_ref().unwrap();
        assert_eq!(output.server_id, "github");
        assert_eq!(output.tool, "create_issue");
        assert_eq!(output.outcome, Ok(call_res));

        // Error outcome replaces output and clears matching in-flight state.
        st.tool_calling = Some(("github".into(), "create_issue".into()));
        st.apply_tool_called(
            "github".into(),
            "create_issue".into(),
            Err("tool denied".into()),
        );
        assert!(st.tool_calling.is_none());
        assert_eq!(
            st.tool_call_output.as_ref().unwrap().outcome,
            Err("tool denied".into())
        );

        // Clicking same tool again collapses inspector.
        st.toggle_inspected_tool("github", &tool);
        assert!(st.inspected_tool.is_none());
    }
}
