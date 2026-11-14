//! The settings page's view of the gateway's per-agent config blocks: which
//! fields each agent has, and typed get/set over them, so the page is a loop
//! over a table and the rules (what is a valid value) are testable here.
//!
//! The blocks themselves are the gateway's own `*Config` types, serialized
//! exactly as its `config.json` names them. Secrets that block holds (T3
//! bearer, DeepSeek token and secret) have no field here: the page never
//! shows them; `config.json` is where they are managed.

use std::path::Path;

use crate::config::AgentConfigs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Binary,
    Autostart,
    /// T3's server.
    Url,
    /// DeepSeek's harness.
    Endpoint,
    RuntimeMode,
    AcpPackage,
    SessionDir,
    Approvals,
    IdleTimeout,
    /// T3's one-time pairing token (`t3 pair`).
    PairingToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// Whole seconds.
    Number,
    Flag,
    Choice(&'static [&'static str]),
}

const T3_MODES: &[&str] = &[
    "default",
    "full-access",
    "approval-required",
    "auto-accept-edits",
    "auto",
];
const PI_APPROVALS: &[&str] = &["ask", "never"];

impl Field {
    pub fn kind(self) -> Kind {
        match self {
            Self::Autostart => Kind::Flag,
            Self::IdleTimeout => Kind::Number,
            Self::RuntimeMode => Kind::Choice(T3_MODES),
            Self::Approvals => Kind::Choice(PI_APPROVALS),
            _ => Kind::Text,
        }
    }

    /// The `config.json` key.
    pub fn key(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Autostart => "autostart",
            Self::Url => "url",
            Self::Endpoint => "endpoint",
            Self::RuntimeMode => "runtime_mode",
            Self::AcpPackage => "acp_package",
            Self::SessionDir => "session_dir",
            Self::Approvals => "approvals",
            Self::IdleTimeout => "idle_timeout_secs",
            Self::PairingToken => "pairing_token",
        }
    }
}

/// The fields `agent` has besides its enable switch, in display order.
pub fn fields(agent: &str) -> &'static [Field] {
    use Field::*;
    match agent {
        "codex" => &[Binary],
        "opencode" => &[Autostart, Binary],
        // Autostart is not offered: the desktop only ever joins a harness that
        // is already running (architecture §4), whatever the file says.
        "deepseek" => &[Endpoint],
        "t3" => &[Url, RuntimeMode],
        "fx" | "qoder" | "cursor" | "antigravity" => &[Binary, IdleTimeout],
        "pi" => &[Binary, SessionDir, Approvals, IdleTimeout],
        "claude" => &[Binary, AcpPackage, IdleTimeout],
        _ => &[],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldError {
    NotAbsolute,
    NotHttp,
    NotANumber,
    NotAChoice,
    NoSuchField,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAbsolute => "Use an absolute path, or leave it empty to look it up on PATH",
            Self::NotHttp => "Use an address starting with http:// or https://",
            Self::NotANumber => "Use a whole number of seconds, 1 or more",
            Self::NotAChoice => "Not one of the choices",
            Self::NoSuchField => "This agent has no such setting",
        })
    }
}

/// Agents whose `binary` may be a bare program name looked up on PATH; every
/// other agent's binary must be an absolute path (the gateway refuses
/// otherwise, e.g. `codex.binary must name an absolute executable path`).
fn bare_binary_allowed(agent: &str) -> bool {
    matches!(agent, "pi" | "opencode")
}

fn optional(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn http_address(text: &str) -> Result<Option<String>, FieldError> {
    match optional(text) {
        Some(url) if !(url.starts_with("http://") || url.starts_with("https://")) => {
            Err(FieldError::NotHttp)
        }
        other => Ok(other),
    }
}

fn seconds(text: &str) -> Result<u64, FieldError> {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or(FieldError::NotANumber)
}

impl AgentConfigs {
    pub fn enabled(&self, agent: &str) -> bool {
        match agent {
            "codex" => self.codex.enabled,
            "opencode" => self.opencode.enabled,
            "deepseek" => self.deepseek.enabled,
            // The gateway's own rule: a URL alone turns T3 on.
            "t3" => self.t3.wanted(),
            "fx" => self.fx.enabled,
            "qoder" => self.qoder.enabled,
            "pi" => self.pi.enabled,
            "claude" => self.claude.enabled,
            "antigravity" => self.antigravity.enabled,
            "cursor" => self.cursor.enabled,
            _ => false,
        }
    }

    pub fn set_enabled(&mut self, agent: &str, on: bool) {
        match agent {
            "codex" => self.codex.enabled = on,
            "opencode" => self.opencode.enabled = on,
            "deepseek" => self.deepseek.enabled = on,
            "t3" => {
                self.t3.enabled = on;
                if !on {
                    // `wanted()` is `enabled || url.is_some()`: off must also
                    // drop the URL, or T3 would stay on.
                    self.t3.url = None;
                }
            }
            "fx" => self.fx.enabled = on,
            "qoder" => self.qoder.enabled = on,
            "pi" => self.pi.enabled = on,
            "claude" => self.claude.enabled = on,
            "antigravity" => self.antigravity.enabled = on,
            "cursor" => self.cursor.enabled = on,
            _ => {}
        }
    }

    /// The field as text: a flag is `true`/`false`, an unset optional is
    /// empty, a choice with no value stored is its default.
    pub fn value(&self, agent: &str, field: Field) -> String {
        let opt = |v: &Option<String>| v.clone().unwrap_or_default();
        match (agent, field) {
            ("codex", Field::Binary) => opt(&self.codex.binary),
            ("opencode", Field::Binary) => opt(&self.opencode.binary),
            ("opencode", Field::Autostart) => self.opencode.autostart.to_string(),
            ("deepseek", Field::Endpoint) => opt(&self.deepseek.endpoint),
            ("t3", Field::Url) => opt(&self.t3.url),
            ("t3", Field::RuntimeMode) => self
                .t3
                .runtime_mode
                .clone()
                .unwrap_or_else(|| "default".into()),
            ("t3", Field::PairingToken) => opt(&self.t3.pairing_token),
            ("fx", Field::Binary) => opt(&self.fx.binary),
            ("fx", Field::IdleTimeout) => self.fx.idle_timeout_secs.to_string(),
            ("qoder", Field::Binary) => opt(&self.qoder.binary),
            ("qoder", Field::IdleTimeout) => self.qoder.idle_timeout_secs.to_string(),
            ("cursor", Field::Binary) => opt(&self.cursor.binary),
            ("cursor", Field::IdleTimeout) => self.cursor.idle_timeout_secs.to_string(),
            ("antigravity", Field::Binary) => opt(&self.antigravity.binary),
            ("antigravity", Field::IdleTimeout) => self.antigravity.idle_timeout_secs.to_string(),
            ("pi", Field::Binary) => opt(&self.pi.binary),
            ("pi", Field::SessionDir) => opt(&self.pi.session_dir),
            ("pi", Field::Approvals) => self.pi.approvals.clone(),
            ("pi", Field::IdleTimeout) => self.pi.idle_timeout_secs.to_string(),
            ("claude", Field::Binary) => opt(&self.claude.binary),
            ("claude", Field::AcpPackage) => opt(&self.claude.acp_package),
            ("claude", Field::IdleTimeout) => self.claude.idle_timeout_secs.to_string(),
            _ => String::new(),
        }
    }

    /// Check `text` for `agent`'s `field` without keeping it.
    pub fn check(agent: &str, field: Field, text: &str) -> Result<(), FieldError> {
        Self::default().set_value(agent, field, text)
    }

    /// Set the field from text. On error nothing changes.
    pub fn set_value(&mut self, agent: &str, field: Field, text: &str) -> Result<(), FieldError> {
        if !(fields(agent).contains(&field) || (agent == "t3" && field == Field::PairingToken)) {
            return Err(FieldError::NoSuchField);
        }
        let binary = |text: &str| -> Result<Option<String>, FieldError> {
            match optional(text) {
                Some(path) if !bare_binary_allowed(agent) && !Path::new(&path).is_absolute() => {
                    Err(FieldError::NotAbsolute)
                }
                other => Ok(other),
            }
        };
        let choice = |text: &str, options: &[&str]| -> Result<String, FieldError> {
            let text = text.trim();
            options
                .contains(&text)
                .then(|| text.to_string())
                .ok_or(FieldError::NotAChoice)
        };
        match (agent, field) {
            ("codex", Field::Binary) => self.codex.binary = binary(text)?,
            ("opencode", Field::Binary) => self.opencode.binary = binary(text)?,
            ("opencode", Field::Autostart) => self.opencode.autostart = text == "true",
            ("deepseek", Field::Endpoint) => self.deepseek.endpoint = http_address(text)?,
            ("t3", Field::Url) => self.t3.url = http_address(text)?,
            ("t3", Field::RuntimeMode) => {
                let mode = choice(text, T3_MODES)?;
                self.t3.runtime_mode = (mode != "default").then_some(mode);
            }
            ("t3", Field::PairingToken) => self.t3.pairing_token = optional(text),
            ("fx", Field::Binary) => self.fx.binary = binary(text)?,
            ("fx", Field::IdleTimeout) => self.fx.idle_timeout_secs = seconds(text)?,
            ("qoder", Field::Binary) => self.qoder.binary = binary(text)?,
            ("qoder", Field::IdleTimeout) => self.qoder.idle_timeout_secs = seconds(text)?,
            ("cursor", Field::Binary) => self.cursor.binary = binary(text)?,
            ("cursor", Field::IdleTimeout) => self.cursor.idle_timeout_secs = seconds(text)?,
            ("antigravity", Field::Binary) => self.antigravity.binary = binary(text)?,
            ("antigravity", Field::IdleTimeout) => {
                self.antigravity.idle_timeout_secs = seconds(text)?
            }
            ("pi", Field::Binary) => self.pi.binary = binary(text)?,
            ("pi", Field::SessionDir) => self.pi.session_dir = optional(text),
            ("pi", Field::Approvals) => self.pi.approvals = choice(text, PI_APPROVALS)?,
            ("pi", Field::IdleTimeout) => self.pi.idle_timeout_secs = seconds(text)?,
            ("claude", Field::Binary) => self.claude.binary = binary(text)?,
            ("claude", Field::AcpPackage) => self.claude.acp_package = optional(text),
            ("claude", Field::IdleTimeout) => self.claude.idle_timeout_secs = seconds(text)?,
            _ => return Err(FieldError::NoSuchField),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_field_reads_and_writes_back() {
        let mut agents = AgentConfigs::default();
        for agent in crate::hub::AGENT_PREFERENCE {
            for field in fields(agent) {
                let before = agents.value(agent, *field);
                agents
                    .set_value(agent, *field, &before)
                    .unwrap_or_else(|e| panic!("{agent} {field:?} {before:?}: {e}"));
                assert_eq!(agents.value(agent, *field), before, "{agent} {field:?}");
            }
        }
    }

    #[test]
    fn binaries_must_be_absolute_except_where_the_gateway_looks_them_up() {
        let mut agents = AgentConfigs::default();
        assert_eq!(
            agents.set_value("codex", Field::Binary, "codex"),
            Err(FieldError::NotAbsolute)
        );
        assert!(
            agents.codex.binary.is_none(),
            "an invalid value is not kept"
        );
        agents
            .set_value("codex", Field::Binary, " /opt/codex ")
            .unwrap();
        assert_eq!(agents.codex.binary.as_deref(), Some("/opt/codex"));
        agents.set_value("codex", Field::Binary, "").unwrap();
        assert!(agents.codex.binary.is_none(), "empty goes back to PATH");
        agents.set_value("pi", Field::Binary, "pi").unwrap();
        assert_eq!(agents.pi.binary.as_deref(), Some("pi"));
    }

    #[test]
    fn numbers_choices_and_addresses_are_checked() {
        let mut agents = AgentConfigs::default();
        assert_eq!(
            agents.set_value("claude", Field::IdleTimeout, "0"),
            Err(FieldError::NotANumber)
        );
        assert_eq!(
            agents.set_value("claude", Field::IdleTimeout, "soon"),
            Err(FieldError::NotANumber)
        );
        agents
            .set_value("claude", Field::IdleTimeout, "90")
            .unwrap();
        assert_eq!(agents.claude.idle_timeout_secs, 90);
        assert_eq!(
            agents.set_value("pi", Field::Approvals, "sometimes"),
            Err(FieldError::NotAChoice)
        );
        assert_eq!(
            agents.set_value("t3", Field::Url, "localhost:3773"),
            Err(FieldError::NotHttp)
        );
        agents
            .set_value("t3", Field::RuntimeMode, "approval-required")
            .unwrap();
        agents
            .set_value("t3", Field::RuntimeMode, "default")
            .unwrap();
        assert!(agents.t3.runtime_mode.is_none());
        assert_eq!(
            agents.set_value("codex", Field::IdleTimeout, "5"),
            Err(FieldError::NoSuchField)
        );
    }

    #[test]
    fn turning_t3_off_also_drops_the_url_that_would_keep_it_on() {
        let mut agents = AgentConfigs::default();
        agents
            .set_value("t3", Field::Url, "http://127.0.0.1:3773")
            .unwrap();
        assert!(agents.enabled("t3"), "a URL alone turns T3 on");
        agents.set_enabled("t3", false);
        assert!(!agents.enabled("t3"));
        assert!(agents.t3.url.is_none());
    }

    #[test]
    fn the_pairing_token_is_not_a_listed_field_but_can_be_set() {
        let mut agents = AgentConfigs::default();
        assert!(!fields("t3").contains(&Field::PairingToken));
        agents
            .set_value("t3", Field::PairingToken, "  abc  ")
            .unwrap();
        assert_eq!(agents.t3.pairing_token.as_deref(), Some("abc"));
        assert_eq!(
            agents.set_value("codex", Field::PairingToken, "x"),
            Err(FieldError::NoSuchField)
        );
    }
}
