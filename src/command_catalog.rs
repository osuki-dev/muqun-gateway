//! One downloaded snapshot of the public agent-command catalog per agent.
//! Reads are local; only `commands update` contacts the catalog website.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context as _};
use serde_json::Value;

const BASE_URL: &str = "https://agent-commands.muqun.dev/catalog";
const CACHE_DIR: &str = "command-catalog";
const MAX_CATALOG_BYTES: usize = 256 * 1024;

const CATALOGS: &[(&str, &[&str])] = &[
    ("claude-code", &["claude"]),
    ("codex-cli", &["codex"]),
    ("opencode", &["opencode", "open-code"]),
    ("qoder-cli", &["qoder"]),
    ("pi", &["pi"]),
    ("copilot", &["copilot"]),
    ("droid", &["droid"]),
    ("kilo", &["kilo"]),
    ("qwen", &["qwen"]),
    ("cursor", &["cursor"]),
    ("antigravity-cli", &["antigravity", "agy"]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogCommand {
    pub name: String,
    pub description: String,
    pub args_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub agent: String,
    pub version: String,
    pub commands: Vec<CatalogCommand>,
}

pub fn catalog_id(profile: &str) -> Option<&'static str> {
    match profile {
        "claude" | "claude-code" => Some("claude-code"),
        "codex" | "codex-cli" => Some("codex-cli"),
        "qoder" | "qodercli" | "qoder-cli" => Some("qoder-cli"),
        _ => CATALOGS
            .iter()
            .find_map(|(id, _)| (*id == profile).then_some(*id)),
    }
}

pub fn profile_for(agent: &str) -> Option<&'static str> {
    let name = agent.trim().to_ascii_lowercase();
    CATALOGS.iter().find_map(|(id, matches)| {
        matches
            .iter()
            .any(|part| {
                if *part == "pi" || *part == "agy" {
                    name == *part || name.starts_with(&format!("{part} "))
                } else {
                    name.contains(part)
                }
            })
            .then_some(*id)
    })
}

fn valid_command_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 80
        && rest
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn parse(id: &str, bytes: &[u8]) -> anyhow::Result<Snapshot> {
    if bytes.len() > MAX_CATALOG_BYTES {
        bail!("catalog is too large");
    }
    let value: Value = serde_json::from_slice(bytes).context("invalid catalog JSON")?;
    if value.get("schemaVersion").and_then(Value::as_u64) != Some(1)
        || value.get("agent").and_then(Value::as_str) != Some(id)
    {
        bail!("catalog header does not match {id}");
    }
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty() && version.len() <= 80)
        .context("invalid catalog version")?
        .to_owned();
    let rows = value
        .get("commands")
        .and_then(Value::as_array)
        .filter(|rows| !rows.is_empty() && rows.len() <= 500)
        .context("invalid command list")?;
    let mut commands = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_command_name(name))
            .context("invalid command name")?;
        if !seen.insert(name) {
            bail!("duplicate command name");
        }
        let description = row
            .get("description")
            .and_then(Value::as_str)
            .filter(|text| {
                !text.is_empty()
                    && text.chars().count() <= 300
                    && !text.chars().any(char::is_control)
            })
            .context("invalid command description")?;
        let args_hint = match row.get("argsHint") {
            None | Some(Value::Null) => None,
            Some(Value::String(text))
                if text.chars().count() <= 120 && !text.chars().any(char::is_control) =>
            {
                Some(text.clone())
            }
            _ => bail!("invalid command argument hint"),
        };
        commands.push(CatalogCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            args_hint,
        });
    }
    Ok(Snapshot {
        agent: id.to_owned(),
        version,
        commands,
    })
}

fn cache_path(root: &Path, id: &str) -> PathBuf {
    root.join(CACHE_DIR).join(format!("{id}.json"))
}

pub fn load(profile: &str) -> Option<Snapshot> {
    let id = catalog_id(profile)?;
    let root = crate::config_dir().ok()?;
    let bytes = std::fs::read(cache_path(&root, id)).ok()?;
    parse(id, &bytes).ok()
}

pub fn is_cached() -> bool {
    CATALOGS.iter().all(|(id, _)| load(id).is_some())
}

async fn fetch(client: &reqwest::Client, id: &str) -> anyhow::Result<(String, Vec<u8>)> {
    let url = format!("{BASE_URL}/{id}.json");
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
            bail!("catalog {id} exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    parse(id, &bytes).with_context(|| format!("invalid catalog for {id}"))?;
    Ok((id.to_owned(), bytes))
}

pub async fn update() -> anyhow::Result<usize> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let downloaded =
        futures::future::try_join_all(CATALOGS.iter().map(|(id, _)| fetch(&client, id))).await?;
    let root = crate::config_dir()?;
    let directory = root.join(CACHE_DIR);
    std::fs::create_dir_all(&directory)?;
    for (id, bytes) in &downloaded {
        let target = cache_path(&root, id);
        let temporary = directory.join(format!("{id}.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temporary, bytes)?;
        if let Err(error) = std::fs::rename(&temporary, &target) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("failed to update {id}"));
        }
    }
    Ok(downloaded.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_catalog_and_rejects_untrusted_commands() {
        let valid = br#"{"schemaVersion":1,"agent":"opencode","version":"2.0.15","commands":[{"name":"/model","description":"Switch model","argsHint":null}]}"#;
        assert_eq!(parse("opencode", valid).unwrap().commands[0].name, "/model");
        assert!(parse("codex-cli", valid).is_err());
        let invalid = br#"{"schemaVersion":1,"agent":"opencode","version":"2.0.15","commands":[{"name":"/bad;rm","description":"Unsafe","argsHint":null}]}"#;
        assert!(parse("opencode", invalid).is_err());
    }

    #[test]
    fn recognizes_the_supported_agent_names() {
        assert_eq!(profile_for("Claude Code"), Some("claude-code"));
        assert_eq!(profile_for("qodercli"), Some("qoder-cli"));
        assert_eq!(profile_for("pi"), Some("pi"));
        assert_eq!(profile_for("copilot"), Some("copilot"));
        assert_eq!(profile_for("agy"), Some("antigravity-cli"));
        assert_eq!(profile_for("strategy"), None);
        assert_eq!(profile_for("openpilot"), None);
    }
}
