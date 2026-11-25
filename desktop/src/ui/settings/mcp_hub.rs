//! Settings → MCP Hub: manage external upstream MCP servers hosted once in
//! Muqun Gateway and exposed to every connected agent via Tool Search
//! (`hub.search_tools` / `hub.call_tool`).

use std::collections::{BTreeMap, HashSet};

use rune::{col, el, row as hrow, text, El, TextEdit};
use rune_kit::{ButtonVariant, Kit, TextInput};

use super::{button, choice, empty, field, flag, group, group_with, row, status, Tone};
use crate::hub::{McpHubError, McpUpstreamTransport, McpUpstreamUpsert, McpUpstreamView};
use crate::model::commands::Command;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

#[cfg(test)]
const MASKED_SECRET: &str = "••••••••";
const SCOPES: [&str; 3] = ["local_only", "tasks", "all"];
const ISOLATIONS: [&str; 2] = ["shared", "per_session"];

pub struct McpHubState {
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
    pub form_saving: bool,
    pub form_error: Option<String>,
}

impl Default for McpHubState {
    fn default() -> Self {
        Self {
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
            form_saving: false,
            form_error: None,
        }
    }
}

impl McpHubState {
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

        Ok((
            id,
            McpUpstreamUpsert {
                label,
                enabled,
                scope: SCOPES[self.scope_idx.min(SCOPES.len() - 1)].to_owned(),
                isolation: ISOLATIONS[self.isolation_idx.min(ISOLATIONS.len() - 1)].to_owned(),
                timeout_s,
                transport,
            },
        ))
    }

    pub fn apply_loaded(&mut self, result: Result<Vec<McpUpstreamView>, McpHubError>) {
        self.loading = false;
        match result {
            Ok(list) => {
                self.upstreams = list;
                self.unsupported_reason = None;
                self.error = None;
            }
            Err(McpHubError::Unsupported(reason)) => {
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

fn upsert_from_view(view: &McpUpstreamView) -> McpUpstreamUpsert {
    McpUpstreamUpsert {
        label: view.label.clone(),
        enabled: view.enabled,
        scope: view.scope.clone(),
        isolation: view.isolation.clone(),
        timeout_s: Some(view.timeout_s),
        transport: view.transport.clone(),
    }
}

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
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

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    let lang = s.cfg.general.language;
    let t = |en| tr_ref(lang, en);
    let mut sections = Vec::new();

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
        let preview_names: Vec<&str> = upstream
            .tools
            .iter()
            .take(12)
            .map(|tool| tool.name.as_str())
            .collect();
        let mut summary = preview_names.join(", ");
        if upstream.tool_count > preview_names.len() {
            summary.push_str(&format!(
                " (+{})",
                upstream.tool_count - preview_names.len()
            ));
        }
        rows.push(
            col()
                .key(format!("mcp-tools-{id}"))
                .w_full()
                .py(8.0)
                .gap(4.0)
                .child(
                    text(format!(
                        "{} ({})",
                        t("Discovered tools"),
                        upstream.tool_count
                    ))
                    .font(k.ty.small)
                    .color(k.c.text_base),
                )
                .child(
                    text(summary)
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap()
                        .w_full(),
                ),
        );
    }

    group_with(k, &format!("mcp-upstream-{id}"), head, rows)
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

        st.apply_loaded(Ok(Vec::new()));
        assert!(st.unsupported_reason.is_none());

        let view = McpUpstreamView {
            id: "demo".into(),
            label: None,
            enabled: true,
            scope: "local_only".into(),
            isolation: "shared".into(),
            timeout_s: 30,
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
}
