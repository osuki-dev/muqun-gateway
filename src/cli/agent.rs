//! `muqun-gateway agent`: each agent's status and the one step that gets it
//! working, without asking the running gateway anything. The probe is the
//! one discovery runs, in this process, with nothing attached.

use clap::Subcommand;

use crate::agents::runtime::resolve_binary;
use crate::agents::AgentRuntime;
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo};
use crate::{load_config, state_dir, Config};

const DSH_START: &str = "bunx @deepseek-ai/dsh web --no-open";

#[derive(Subcommand)]
pub(crate) enum AgentCommand {
    /// Each agent's status and the next step (the default).
    List,
}

pub(crate) async fn run_agent_command(
    command: Option<AgentCommand>,
    json: bool,
) -> anyhow::Result<()> {
    match command.unwrap_or(AgentCommand::List) {
        AgentCommand::List => {
            if !list(json).await? {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// Print every agent; `true` when each enabled one is usable.
async fn list(json: bool) -> anyhow::Result<bool> {
    let config = load_config(None)?;
    let runtime = AgentRuntime::with_configs(
        config.opencode.clone(),
        config.deepseek.clone(),
        config.t3.clone(),
        Some(state_dir()?),
    );
    // The uncached probe: `discover_agents` gives up after a few seconds
    // and would report everything offline while a catalog is still loading.
    let agents = runtime.probe_agents().await.agents;
    if json {
        println!("{}", serde_json::to_string_pretty(&agents)?);
    } else {
        for agent in &agents {
            println!("{}", format_row(agent, &config));
        }
    }
    Ok(all_ready(
        agents.iter().map(|agent| (agent.enabled, &agent.status)),
    ))
}

fn format_row(agent: &AgentDiscoveryInfo, config: &Config) -> String {
    let location = agent.endpoint.clone().or_else(|| {
        (agent.id == "opencode")
            .then(|| resolve_binary(config.opencode.binary.as_deref()).ok())
            .flatten()
            .map(|path| path.display().to_string())
    });
    let next = next_step(&agent.id, &agent.status)
        .map(|step| format!("  → {step}"))
        .unwrap_or_default();
    format!(
        "{:<9} {:<13} {:<9} {}{next}",
        agent.id,
        status_name(&agent.status),
        agent.version.as_deref().unwrap_or("-"),
        location.as_deref().unwrap_or("-"),
    )
}

/// The wire name of a status, as discovery spells it.
fn status_name(status: &AgentAvailability) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// What to run so that agent `id` in `status` becomes usable; `None` when it
/// already is.
pub(crate) fn next_step(id: &str, status: &AgentAvailability) -> Option<&'static str> {
    use AgentAvailability::*;
    Some(match (status, id) {
        (Connected | Reachable, _) => return None,
        (Unconfigured | Disabled, "t3") => "muqun-gateway agent setup t3",
        (Unconfigured | Disabled, "deepseek") => "muqun-gateway agent setup deepseek",
        (Disabled, "opencode") => "set `opencode.enabled` to true in config.json",
        (Offline, "opencode") => "opencode service start",
        (Offline | NotInstalled, "deepseek") => DSH_START,
        (Offline, "t3") => "open T3 Code, or run `t3 service install`",
        (NotInstalled, "opencode") => "install OpenCode 2 (https://opencode.ai)",
        (NotInstalled, "t3") => "install T3 Code (https://t3.codes)",
        _ => return None,
    })
}

/// Whether every enabled agent is connected or reachable.
pub(crate) fn all_ready<'a>(
    agents: impl IntoIterator<Item = (bool, &'a AgentAvailability)>,
) -> bool {
    agents
        .into_iter()
        .filter(|(enabled, _)| *enabled)
        .all(|(_, status)| {
            matches!(
                status,
                AgentAvailability::Connected | AgentAvailability::Reachable
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_step_names_the_command_for_each_status() {
        use AgentAvailability::*;
        let cases = [
            ("t3", Unconfigured, Some("muqun-gateway agent setup t3")),
            ("t3", Disabled, Some("muqun-gateway agent setup t3")),
            (
                "deepseek",
                Disabled,
                Some("muqun-gateway agent setup deepseek"),
            ),
            ("opencode", Offline, Some("opencode service start")),
            ("deepseek", Offline, Some(DSH_START)),
            (
                "t3",
                Offline,
                Some("open T3 Code, or run `t3 service install`"),
            ),
            (
                "opencode",
                NotInstalled,
                Some("install OpenCode 2 (https://opencode.ai)"),
            ),
            (
                "t3",
                NotInstalled,
                Some("install T3 Code (https://t3.codes)"),
            ),
            ("opencode", Connected, None),
            ("t3", Reachable, None),
            ("other", Offline, None),
        ];
        for (id, status, want) in cases {
            assert_eq!(next_step(id, &status), want, "{id} {status:?}");
        }
    }

    #[test]
    fn only_enabled_agents_decide_readiness() {
        use AgentAvailability::*;
        assert!(all_ready([
            (true, &Reachable),
            (false, &Disabled),
            (true, &Connected)
        ]));
        assert!(!all_ready([(true, &Reachable), (true, &Unconfigured)]));
        assert!(!all_ready([(true, &Offline)]));
    }

    #[test]
    fn status_names_match_the_wire() {
        assert_eq!(
            status_name(&AgentAvailability::NotInstalled),
            "not_installed"
        );
    }
}
