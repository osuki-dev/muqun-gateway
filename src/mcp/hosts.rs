//! The MCP client config `muqun-gateway mcp enable <host>` writes or prints.
//!
//! Pure functions over JSON documents; reading and writing the files is
//! `platform/store.rs`'s. Entries never hold a token: Claude Code, Codex and
//! Pi run the token helper for fresh headers (Claude Code and Codex re-run it
//! after a 401 or 403); OpenCode reads `MUQUN_MCP_TOKEN`,
//! which the gateway sets for the OpenCode service it starts.

use serde_json::{json, Map, Value};

pub(crate) const SERVER_NAME: &str = "muqun";
pub(crate) const TOKEN_ENV: &str = "MUQUN_MCP_TOKEN";
/// The command hosts run to get headers (JSON object on stdout).
pub(crate) const HELPER: &str = "muqun-gateway mcp token-helper";

/// The helper command for a gateway whose config lives in `config_dir`, or
/// in the default place when `None`. Hosts may run the helper with a
/// stripped environment, so a non-default directory is passed as a flag
/// rather than trusted to `MUQUN_GATEWAY_CONFIG_DIR`.
pub(crate) fn helper_command(config_dir: Option<&std::path::Path>) -> String {
    match config_dir {
        None => HELPER.to_owned(),
        Some(dir) => format!("{HELPER} --config-dir {}", quoted(&dir.to_string_lossy())),
    }
}

/// `resolved`, made absolute, when it is not `default`: the directory the
/// helper must be told about, because a host may run it without our
/// environment (and from another working directory). A path that cannot be
/// made absolute is passed on as it is.
pub(crate) fn non_default_dir(
    resolved: std::path::PathBuf,
    default: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
    (resolved != default).then_some(resolved)
}

/// The helper command for this gateway's config directory, told the exact
/// `/mcp` address the host is given, which it checks before any token goes
/// there.
pub(crate) fn helper_for_url(url: &str) -> String {
    format!("{} --url {}", helper_for_this_gateway(), quoted(url))
}

/// The helper command for this gateway's config directory.
pub(crate) fn helper_for_this_gateway() -> String {
    let baked = match (
        crate::config_dir(),
        crate::platform::store::standalone_config_dir(),
    ) {
        (Ok(resolved), Ok(default)) => non_default_dir(resolved, &default),
        _ => None,
    };
    helper_command(baked.as_deref())
}

/// `text` as one shell word: bare when it is plainly safe, else single-quoted.
fn quoted(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%".contains(c));
    if plain {
        text.to_owned()
    } else {
        crate::terminal::shell_word::shell_word(text)
    }
}

/// Pi's `!command` header value: the command's stdout is the whole value.
fn pi_header(helper: &str) -> String {
    format!("!echo Bearer $({helper} --bearer)")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Host {
    Claude,
    Opencode,
    Codex,
    Pi,
}

impl Host {
    /// The project file this host reads, for hosts the gateway writes to.
    pub(crate) fn project_file(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some(".mcp.json"),
            Self::Opencode => Some("opencode.json"),
            Self::Codex | Self::Pi => None,
        }
    }

    /// The object in that file that holds MCP servers by name.
    fn section(self) -> &'static str {
        match self {
            Self::Claude => "mcpServers",
            _ => "mcp",
        }
    }

    /// The `muqun` server entry; `helper` is [`helper_command`]'s answer.
    pub(crate) fn entry(self, url: &str, helper: &str) -> Value {
        match self {
            Self::Claude => json!({
                "type": "http",
                "url": url,
                "headersHelper": helper,
            }),
            // OpenCode substitutes `{env:VAR}` in any config string.
            _ => json!({
                "type": "remote",
                "url": url,
                "enabled": true,
                "oauth": false,
                "headers": { "Authorization": format!("Bearer {{env:{TOKEN_ENV}}}") },
            }),
        }
    }
}

/// Put `muqun` into `doc`'s server section, leaving every other key alone.
/// Returns whether anything changed, so a second run is a no-op.
pub(crate) fn merge_entry(
    doc: &mut Map<String, Value>,
    host: Host,
    url: &str,
    helper: &str,
) -> anyhow::Result<bool> {
    let section = doc
        .entry(host.section())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = section else {
        anyhow::bail!("\"{}\" is not an object; left untouched", host.section());
    };
    let entry = host.entry(url, helper);
    if servers.get(SERVER_NAME) == Some(&entry) {
        return Ok(false);
    }
    servers.insert(SERVER_NAME.into(), entry);
    Ok(true)
}

/// Take `muqun` out of `doc`, and nothing else. Returns whether it was there.
pub(crate) fn remove_entry(doc: &mut Map<String, Value>, host: Host) -> bool {
    doc.get_mut(host.section())
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.remove(SERVER_NAME))
        .is_some()
}

/// Instructions for hosts (or scopes) the gateway does not write to.
pub(crate) fn manual_steps(host: Host, url: &str, helper: &str) -> String {
    match host {
        Host::Claude => format!(
            "Project scope: re-run with --project <dir> to write <dir>/.mcp.json.\n\
             User scope (`claude mcp add` has no helper flag, so use add-json):\n\
             claude mcp add-json --scope user {SERVER_NAME} {}",
            crate::terminal::shell_word::shell_word(&host.entry(url, helper).to_string())
        ),
        Host::Opencode => format!(
            "Add to ~/.config/opencode/opencode.json:\n{}",
            serde_json::to_string_pretty(
                &json!({ "mcp": { SERVER_NAME: host.entry(url, helper) } })
            )
            .unwrap_or_default()
        ),
        Host::Codex => format!(
            "Add to ~/.codex/config.toml:\n\n[mcp_servers.{SERVER_NAME}]\nurl = {}\n\
             http_headers_helper = {}",
            toml_string(url),
            toml_string(helper)
        ),
        Host::Pi => format!(
            "Pi runs a header value that starts with \"!\" as a command. Add this to \
             ~/.pi/agent/mcp.json (all projects) or .pi/mcp.json (this project):\n{}",
            serde_json::to_string_pretty(&json!({ "mcpServers": { SERVER_NAME: {
                "url": url, "headers": { "Authorization": pi_header(helper) }
            } } }))
            .unwrap_or_default()
        ),
    }
}

/// A TOML basic string: JSON's escaping is a subset TOML reads the same way.
fn toml_string(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

pub(crate) fn manual_removal(host: Host) -> String {
    match host {
        Host::Claude => format!("claude mcp remove --scope user {SERVER_NAME}"),
        Host::Opencode => {
            format!("Remove \"mcp.{SERVER_NAME}\" from ~/.config/opencode/opencode.json")
        }
        Host::Codex => format!("Remove [mcp_servers.{SERVER_NAME}] from ~/.codex/config.toml"),
        Host::Pi => format!(
            "Remove \"mcpServers.{SERVER_NAME}\" from ~/.pi/agent/mcp.json or .pi/mcp.json \
             (or use `pi mcp remove`)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "http://127.0.0.1:23847/mcp";

    fn doc(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn enabling_twice_changes_nothing_the_second_time_and_keeps_other_servers() {
        for host in [Host::Claude, Host::Opencode] {
            let section = host.section();
            let mut file = doc(json!({
                "theme": "dark",
                section: { "other": { "url": "http://example.invalid" } },
            }));
            assert!(merge_entry(&mut file, host, URL, HELPER).unwrap());
            let once = file.clone();
            assert!(!merge_entry(&mut file, host, URL, HELPER).unwrap());
            assert_eq!(file, once);
            assert_eq!(file["theme"], "dark");
            assert!(file[section]["other"].is_object());
            assert_eq!(file[section][SERVER_NAME]["url"], URL);

            assert!(remove_entry(&mut file, host));
            assert!(!remove_entry(&mut file, host));
            assert!(file[section]["other"].is_object());
        }
    }

    #[test]
    fn entries_never_hold_a_token() {
        let claude = Host::Claude.entry(URL, HELPER);
        assert_eq!(claude["headersHelper"], "muqun-gateway mcp token-helper");
        assert!(claude.get("headers").is_none());
        assert_eq!(
            Host::Opencode.entry(URL, HELPER)["headers"]["Authorization"],
            "Bearer {env:MUQUN_MCP_TOKEN}"
        );
        assert!(manual_steps(Host::Codex, URL, HELPER)
            .contains("http_headers_helper = \"muqun-gateway mcp token-helper\""));
        assert!(manual_steps(Host::Pi, URL, HELPER).contains("token-helper --bearer"));
        assert!(manual_steps(Host::Claude, URL, HELPER).contains("add-json --scope user muqun"));
    }

    #[test]
    fn a_section_that_is_not_an_object_is_refused() {
        let mut file = doc(json!({ "mcpServers": [] }));
        assert!(merge_entry(&mut file, Host::Claude, URL, HELPER).is_err());
        assert_eq!(file, doc(json!({ "mcpServers": [] })));
    }

    #[test]
    fn a_non_default_config_dir_is_baked_into_every_helper_command() {
        assert_eq!(helper_command(None), HELPER);
        let plain = helper_command(Some(std::path::Path::new("/srv/gw-config")));
        assert_eq!(
            plain,
            "muqun-gateway mcp token-helper --config-dir /srv/gw-config"
        );
        let spaced = helper_command(Some(std::path::Path::new("/srv/my config")));
        assert_eq!(
            spaced,
            "muqun-gateway mcp token-helper --config-dir '/srv/my config'"
        );
        assert!(manual_steps(Host::Codex, URL, &spaced)
            .contains(r#"http_headers_helper = "muqun-gateway mcp token-helper --config-dir '/srv/my config'""#));
        let pi = manual_steps(Host::Pi, URL, &plain);
        assert!(pi.contains("--config-dir /srv/gw-config --bearer"), "{pi}");
    }

    #[test]
    fn a_relative_config_dir_is_made_absolute() {
        let default = std::path::Path::new("/home/user/.config/muqun-gateway");
        let baked = non_default_dir("demo/cfg".into(), default).unwrap();
        assert!(baked.is_absolute(), "{}", baked.display());
        assert!(baked.ends_with("demo/cfg"));
        assert_eq!(baked, std::env::current_dir().unwrap().join("demo/cfg"));
        assert_eq!(non_default_dir(default.to_path_buf(), default), None);
    }

    #[test]
    fn the_pi_snippet_uses_mcp_servers() {
        let text = manual_steps(Host::Pi, URL, HELPER);
        let json: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
        let entry = &json["mcpServers"][SERVER_NAME];
        assert_eq!(entry["url"], URL);
        assert_eq!(
            entry["headers"]["Authorization"],
            "!echo Bearer $(muqun-gateway mcp token-helper --bearer)"
        );
    }
}
