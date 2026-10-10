//! `muqun-gateway mcp`: host configuration, the token helper hosts run, and
//! token revocation.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use serde_json::{json, Value};

use super::local_api::{admin_request, GatewayNotRunning};
use crate::mcp::hosts::{self, Host, TOKEN_ENV};
use crate::mcp::tokens::AssistantTokenStore as _;

#[derive(Subcommand)]
pub(crate) enum McpCommand {
    /// Print request headers with a fresh assistant token for the terminal
    /// pane this process runs in. Hosts run this; you rarely do.
    TokenHelper {
        /// Print only the token (for hosts that build the header themselves).
        #[arg(long)]
        bearer: bool,
        /// The gateway's config directory, when it is not the default. Hosts
        /// may run the helper with a stripped environment, so `mcp enable`
        /// writes this flag rather than relying on MUQUN_GATEWAY_CONFIG_DIR.
        #[arg(long, value_name = "DIR")]
        config_dir: Option<PathBuf>,
    },
    /// Revoke assistant tokens. A running gateway sees it on the next call.
    #[command(group(clap::ArgGroup::new("which").required(true).args(["all", "id"])))]
    Revoke {
        #[arg(long)]
        all: bool,
        /// A token id from the devices API.
        #[arg(long)]
        id: Option<String>,
    },
    /// Point an MCP host (Claude Code, OpenCode, Codex, Pi) at this gateway.
    Enable {
        #[arg(value_enum)]
        host: Host,
        /// Project directory to write the host's project config into.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Gateway MCP URL. Defaults to the configured listen address (loopback
        /// for a wildcard one) and port.
        #[arg(long)]
        url: Option<String>,
    },
    /// Remove this gateway from a host's project config.
    Disable {
        #[arg(value_enum)]
        host: Host,
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

fn default_url() -> String {
    match crate::load_config(None) {
        Ok(config) => crate::mcp::recorded_url(&config.listen),
        Err(_) => crate::mcp::local_url(""),
    }
}

/// Merge the `muqun` entry into the project file at `path`; what to tell
/// the user.
fn enable_in_project(host: Host, path: &Path, url: &str, helper: &str) -> anyhow::Result<String> {
    let mut doc = crate::read_json_object_at(path)?.unwrap_or_default();
    if hosts::merge_entry(&mut doc, host, url, helper)? {
        crate::write_json_object_at(path, &doc)?;
        Ok(format!(
            "Wrote \"{}\" to {}",
            hosts::SERVER_NAME,
            path.display()
        ))
    } else {
        Ok(format!("{} already points at {url}", path.display()))
    }
}

/// The loopback warning for the configured listen address, if it needs one:
/// not when the running gateway serves `/mcp` on its own loopback listener.
fn listen_warning_for_this_gateway() -> Option<String> {
    let config = crate::load_config(None).ok()?;
    let recorded =
        crate::mcp::recorded_url(&config.listen) != crate::mcp::local_url(&config.listen);
    if recorded {
        return None;
    }
    crate::mcp::loopback_warning(&config.listen)
}

const MAX_ANCESTORS: usize = 12;
const NOT_IN_PANE: &str = "muqun: this process is not inside a terminal pane the gateway manages";
const MCP_DISABLED: &str = "muqun: MCP is disabled on this gateway (mcp.enabled = false)";
const HELPER_FAILED: &str = "muqun: could not get a token from the gateway";

/// The ppid in a `/proc/<pid>/stat` line: `pid (comm) state ppid ...`, where
/// `comm` may contain spaces and parens, so count from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Parent pid of `pid`, or `None` when it has none or cannot be read.
fn parent_pid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_stat_ppid(&stat)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

/// Parents of `start`, nearest first, at most `limit` of them.
fn ancestor_chain(start: u32, limit: usize, parent_of: impl Fn(u32) -> Option<u32>) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = start;
    while chain.len() < limit {
        match parent_of(current) {
            Some(parent) if parent > 0 && !chain.contains(&parent) => {
                chain.push(parent);
                current = parent;
            }
            _ => break,
        }
    }
    chain
}

/// How `token-helper` ends for a given gateway answer: `Ok` carries nothing
/// more to do, `Err((exit_code, message))` is printed to stderr. A 404 that is
/// not `pane_not_found` is the route missing: MCP is switched off.
fn helper_outcome(status: u16, body: &Value) -> Result<(), (i32, String)> {
    match status {
        200 => Ok(()),
        404 if body["error"]["code"] == "pane_not_found" => Err((2, NOT_IN_PANE.into())),
        404 => Err((1, MCP_DISABLED.into())),
        other => Err((
            1,
            format!("muqun: the gateway refused the request (HTTP {other})"),
        )),
    }
}

fn token_helper(bearer: bool, config_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let fail = |code: i32, message: String| -> ! {
        eprintln!("{message}");
        std::process::exit(code)
    };
    if let Some(dir) = config_dir {
        crate::platform::store::override_config_dir(dir);
    }
    // One fixed sentence for anything else: the error chain can name files
    // under the config directory, and this output lands in a host's log.
    // `MUQUN_LOG=debug` shows the details.
    let config = crate::load_config(None).unwrap_or_else(|error| {
        tracing::debug!("token-helper: {error:#}");
        fail(1, HELPER_FAILED.into())
    });
    // Identity before credentials: the host sends this token to the loopback
    // MCP address, so that address must be this gateway, not a process that
    // took over a stale port.
    let url = crate::mcp::recorded_url(&config.listen);
    if let Err(refused) = crate::mcp::verify_identity(&url, &config.server_id) {
        tracing::warn!("{refused}");
        fail(1, refused);
    }
    let pids = ancestor_chain(std::process::id(), MAX_ANCESTORS, parent_pid);
    let body = json!({ "ancestor_pids": pids });
    let (status, value) = match admin_request(&config, "POST", "/api/mcp/tokens", Some(&body)) {
        Ok(reply) => reply,
        Err(error) if error.is::<GatewayNotRunning>() => {
            fail(1, "muqun: the gateway is not running".into())
        }
        Err(error) => {
            tracing::debug!("token-helper: {error:#}");
            fail(1, HELPER_FAILED.into())
        }
    };
    if let Err((code, message)) = helper_outcome(status, &value) {
        fail(code, message);
    }
    let Some(token) = value.get("token").and_then(Value::as_str) else {
        fail(1, "muqun: the gateway sent no token".into())
    };
    if bearer {
        println!("{token}");
    } else {
        // Claude Code `headersHelper` and Codex `http_headers_helper` both
        // read a JSON object of header names to string values.
        println!("{}", json!({ "Authorization": format!("Bearer {token}") }));
    }
    Ok(())
}

async fn revoke(all: bool, id: Option<String>) -> anyhow::Result<()> {
    let config = crate::load_config(None)?;
    let state = crate::state_dir()?;
    let key = crate::platform::store::db_key::key_for(&config.store, &state)?;
    let db = crate::platform::store::sqlite::SqliteStore::open(
        &crate::store_db_path(&state),
        key,
        crate::platform::store::sqlite::ALL_MIGRATIONS,
    )?;
    let store = crate::mcp::tokens_sqlite::SqliteTokenStore::new(db);
    let count = match (all, id) {
        (true, _) => store.revoke_all().await?,
        (false, Some(id)) => usize::from(store.revoke(&id).await?),
        (false, None) => anyhow::bail!("pass --all or --id <token_id>"),
    };
    println!("Revoked {count} assistant token(s). A running gateway sees this on the next call.");
    Ok(())
}

pub(crate) async fn run_mcp_command(command: McpCommand) -> anyhow::Result<()> {
    match command {
        McpCommand::TokenHelper { bearer, config_dir } => token_helper(bearer, config_dir)?,
        McpCommand::Revoke { all, id } => revoke(all, id).await?,
        McpCommand::Enable { host, project, url } => {
            let url = url.unwrap_or_else(default_url);
            let helper = hosts::helper_for_this_gateway();
            if let Some(warning) = listen_warning_for_this_gateway() {
                println!("{warning}");
            }
            match (host.project_file(), project) {
                (Some(file), Some(project)) => {
                    let path = project.join(file);
                    println!("{}", enable_in_project(host, &path, &url, &helper)?);
                }
                _ => println!("{}", hosts::manual_steps(host, &url, &helper)),
            }
            match host {
                Host::Opencode => println!(
                    "\nThe gateway sets ${TOKEN_ENV} for the OpenCode service it starts. \
                     For OpenCode run by hand, inside a terminal pane the gateway manages:\n\
                     export {TOKEN_ENV}=$({helper} --bearer)"
                ),
                _ => println!("\nThe host runs `{helper}` for a fresh token."),
            }
        }
        McpCommand::Disable { host, project } => match (host.project_file(), project) {
            (Some(file), Some(project)) => {
                let path = project.join(file);
                let Some(mut doc) = crate::read_json_object_at(&path)? else {
                    println!("{} does not exist; nothing to remove", path.display());
                    return Ok(());
                };
                if hosts::remove_entry(&mut doc, host) {
                    crate::write_json_object_at(&path, &doc)?;
                    println!("Removed \"{}\" from {}", hosts::SERVER_NAME, path.display());
                } else {
                    println!("{} has no \"{}\" entry", path.display(), hosts::SERVER_NAME);
                }
            }
            _ => println!("{}", hosts::manual_removal(host)),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn chain_starts_at_our_parent_and_is_bounded() {
        let chain = ancestor_chain(std::process::id(), MAX_ANCESTORS, parent_pid);
        assert!(!chain.is_empty());
        assert!(chain.len() <= MAX_ANCESTORS);
        assert_eq!(Some(chain[0]), parent_pid(std::process::id()));
    }

    #[test]
    fn chain_stops_at_the_limit_a_root_and_a_cycle() {
        assert_eq!(ancestor_chain(10, 2, |p| Some(p - 1)), vec![9, 8]);
        assert_eq!(
            ancestor_chain(3, 12, |p| (p > 1).then(|| p - 1)),
            vec![2, 1]
        );
        assert_eq!(ancestor_chain(5, 12, |_| Some(5)), vec![5]);
        assert!(ancestor_chain(5, 12, |_| None).is_empty());
    }

    #[test]
    fn stat_ppid_survives_parens_and_spaces_in_comm() {
        assert_eq!(parse_stat_ppid("1 (my (prog)) S 42 1 1 0"), Some(42));
        assert_eq!(parse_stat_ppid("7 (bash) S 3 7"), Some(3));
        assert_eq!(parse_stat_ppid("garbage"), None);
        assert_eq!(parse_stat_ppid("1 (x) S"), None);
    }

    #[test]
    fn helper_exit_codes_follow_the_gateway_status() {
        let none = Value::Null;
        let pane = json!({ "error": { "code": "pane_not_found", "message": "-" } });
        assert!(helper_outcome(200, &none).is_ok());
        assert_eq!(
            helper_outcome(404, &pane).unwrap_err(),
            (2, NOT_IN_PANE.into())
        );
        // The route itself is missing: MCP is off.
        assert_eq!(
            helper_outcome(404, &none).unwrap_err(),
            (1, MCP_DISABLED.into())
        );
        let other = json!({ "error": { "code": "not_found" } });
        assert_eq!(helper_outcome(404, &other).unwrap_err().0, 1);
        assert_eq!(helper_outcome(401, &none).unwrap_err().0, 1);
        assert_eq!(helper_outcome(500, &none).unwrap_err().0, 1);
    }

    /// `mcp enable claude --project`: the helper carries `--config-dir`
    /// exactly when the resolved config dir is not the default one.
    #[test]
    fn enable_bakes_a_non_default_config_dir_into_the_project_file() {
        let default = Path::new("/home/user/.config/muqun-gateway");
        let project = crate::test_state_dir().join(format!("proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&project).unwrap();
        let url = "http://127.0.0.1:23999/mcp";
        let written = |resolved: PathBuf| {
            let path = project.join(".mcp.json");
            let _ = std::fs::remove_file(&path);
            let baked = hosts::non_default_dir(resolved, default);
            let helper = hosts::helper_command(baked.as_deref());
            enable_in_project(Host::Claude, &path, url, &helper).unwrap();
            let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            doc["mcpServers"]["muqun"]["headersHelper"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            written(PathBuf::from("/tmp/demo/config")),
            "muqun-gateway mcp token-helper --config-dir /tmp/demo/config"
        );
        assert_eq!(
            written(default.to_path_buf()),
            "muqun-gateway mcp token-helper"
        );
    }

    #[test]
    fn token_helper_takes_a_config_dir() {
        use clap::Parser as _;
        let cli = crate::Cli::try_parse_from([
            "muqun-gateway",
            "mcp",
            "token-helper",
            "--bearer",
            "--config-dir",
            "/tmp/demo/config",
        ])
        .unwrap();
        let crate::cli::Command::Mcp {
            command: McpCommand::TokenHelper { bearer, config_dir },
        } = cli.command
        else {
            panic!("not token-helper");
        };
        assert!(bearer);
        assert_eq!(config_dir, Some(PathBuf::from("/tmp/demo/config")));
    }
}
