//! `muqun-gateway peers`: link this computer with the owner's other
//! gateways without a phone, list them, and unlink one.

use std::time::Duration;

use anyhow::Context as _;
use clap::Subcommand;
use serde_json::{json, Value};

use super::local_api::admin_request_with_timeouts;
use crate::fleet::peer_client;

/// Linking claims a code over there (Argon2id on both sides) and makes a
/// first call; give it more than a local request's two seconds.
const LINK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Subcommand)]
pub(crate) enum PeersCommand {
    /// Link this computer with the gateway at <url>: it asks that gateway to
    /// pair, then reads the code `muqun-gateway manage` shows over there.
    Link {
        url: String,
        /// What to call it here; default: its own label.
        #[arg(long)]
        name: Option<String>,
    },
    /// The computers this one is linked with.
    List,
    /// Unlink a computer by name (it is told to forget this one too).
    Remove { name: String },
}

/// The refusal's message, or the status when it has none.
fn refusal(status: u16, value: &Value) -> String {
    value["error"]["message"]
        .as_str()
        .map_or_else(|| format!("the gateway answered {status}"), str::to_owned)
}

fn listed(config: &crate::Config) -> anyhow::Result<Vec<Value>> {
    // `GET /api/peers` pings every stale peer first (3 s each, at once), so
    // an unreachable one takes longer than a local request's two seconds.
    let (status, value) = admin_request_with_timeouts(
        config,
        "GET",
        "/api/peers",
        None,
        Duration::from_secs(1),
        LINK_TIMEOUT,
    )?;
    anyhow::ensure!(status == 200, refusal(status, &value));
    Ok(value["peers"].as_array().cloned().unwrap_or_default())
}

pub(crate) async fn run(command: PeersCommand) -> anyhow::Result<()> {
    let config = crate::load_config(None)?;
    match command {
        PeersCommand::Link { url, name } => {
            let url = url.trim().trim_end_matches('/').to_owned();
            let request_id = format!("peer-{}", uuid::Uuid::new_v4());
            let label =
                peer_client::request_pairing(&url, &request_id, &config.label, &config.server_id)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("{url} did not take the pairing request: {error}")
                    })?;
            println!("Enter the code shown by \"muqun-gateway manage\" on {label}:");
            let mut code = String::new();
            std::io::stdin()
                .read_line(&mut code)
                .context("failed to read the code")?;
            let name = name.unwrap_or(label);
            let body = json!({
                "url": url,
                "name": name,
                "request_id": request_id,
                "code": code.trim(),
            });
            let (status, value) = admin_request_with_timeouts(
                &config,
                "POST",
                "/api/peers",
                Some(&body),
                Duration::from_secs(1),
                LINK_TIMEOUT,
            )?;
            anyhow::ensure!(status == 200, refusal(status, &value));
            println!("linked with {}", value["name"].as_str().unwrap_or(&name));
        }
        PeersCommand::List => {
            let peers = listed(&config)?;
            if peers.is_empty() {
                println!("no linked computers");
            }
            for peer in peers {
                let reachable = if peer["reachable"].as_bool() == Some(true) {
                    "reachable"
                } else {
                    "unreachable"
                };
                println!(
                    "{}  {}  {reachable}",
                    peer["name"].as_str().unwrap_or_default(),
                    peer["url"].as_str().unwrap_or_default()
                );
            }
        }
        PeersCommand::Remove { name } => {
            let peers = listed(&config)?;
            let peer = peers
                .iter()
                .find(|peer| {
                    peer["name"]
                        .as_str()
                        .is_some_and(|known| known.eq_ignore_ascii_case(name.trim()))
                })
                .with_context(|| format!("no linked computer is called {name}"))?;
            let peer_id = peer["peer_id"].as_str().unwrap_or_default();
            let (status, value) = admin_request_with_timeouts(
                &config,
                "DELETE",
                &format!("/api/peers/{peer_id}"),
                None,
                Duration::from_secs(1),
                LINK_TIMEOUT,
            )?;
            anyhow::ensure!(status == 200, refusal(status, &value));
            let name = peer["name"].as_str().unwrap_or(&name);
            println!("unlinked {name}");
            if value["remote_revoked"] != true {
                println!(
                    "{name} did not confirm it revoked this computer; revoke it in its manage view"
                );
            }
            match (&value["counterpart_found"], &value["counterpart_revoked"]) {
                (_, Value::Bool(true)) => {}
                (Value::Bool(true), _) => println!(
                    "{name}'s login here could not be revoked; revoke it in manage > Devices"
                ),
                _ => println!(
                    "{name} holds no login here that it was paired as; check manage > Devices"
                ),
            }
        }
    }
    Ok(())
}
