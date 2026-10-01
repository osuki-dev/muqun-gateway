//! `muqun-gateway agent`: each agent's status and the one step that gets it
//! working, without asking the running gateway anything. The probe is the
//! one discovery runs, in this process, with nothing attached.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{anyhow, Context as _};
use clap::Subcommand;

use crate::agents::adapters::deepseek::{auth::load_local_secret, DeepseekEndpoint};
use crate::agents::adapters::t3::{T3Credential, T3Endpoint};
use crate::agents::runtime::{binary_version, resolve_binary, DEFAULT_T3_URL};
use crate::agents::{AgentRuntime, DeepseekConfig, T3Config};
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo};
use crate::platform::service::SERVICE_LABEL;
use crate::{
    config_dir, load_config, now_unix_ms, state_dir, write_config, write_t3_credential_at, Config,
    T3StoredCredential, CONFIG_FILE,
};

const DSH_START: &str = "bunx @deepseek-ai/dsh web --no-open";
/// How the gateway names itself to an agent it pairs with.
const CLIENT_LABEL: &str = "muqun-gateway";

#[derive(Subcommand)]
pub(crate) enum AgentCommand {
    /// Each agent's status and the next step (the default).
    List,
    /// Configure one agent in config.json and apply it.
    Setup {
        #[command(subcommand)]
        agent: SetupAgent,
    },
}

#[derive(Subcommand)]
pub(crate) enum SetupAgent {
    /// Give the gateway a T3 Code credential and enable T3.
    T3 {
        /// T3 server URL; defaults to `t3.url`, then http://127.0.0.1:3773.
        #[arg(long)]
        url: Option<String>,
        /// A pairing code from `t3 pair`, for a T3 server on another host.
        #[arg(long)]
        token: Option<String>,
        /// T3 Code data directory, passed to `t3 auth session issue`.
        #[arg(long)]
        base_dir: Option<PathBuf>,
        /// Restart the gateway service without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Point the gateway at a running DeepSeek Harness and enable it.
    Deepseek {
        /// Harness URL; defaults to `deepseek.endpoint`, `DSH_URL`, then the
        /// local ports 3080 and 19387.
        #[arg(long)]
        endpoint: Option<String>,
        /// Restart the gateway service without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Show the OpenCode the gateway would use; it needs no setup.
    Opencode,
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
        AgentCommand::Setup { agent } => match agent {
            SetupAgent::T3 {
                url,
                token,
                base_dir,
                yes,
            } => {
                let config_path = config_dir()?.join(CONFIG_FILE);
                let outcome = setup_t3_at(
                    &config_path,
                    &state_dir()?,
                    url,
                    token,
                    which("t3"),
                    base_dir.as_deref(),
                )
                .await?;
                let Some(restart) = outcome else {
                    eprintln!(
                        "No `t3` on PATH: run `t3 pair` on the T3 host and re-run with \
                         `--token <code>`."
                    );
                    std::process::exit(2);
                };
                restart_if_running(
                    restart,
                    yes,
                    "T3 was already on for this URL: the running gateway reads the new \
                     credential on its next round, no restart needed",
                )?;
            }
            SetupAgent::Deepseek { endpoint, yes } => {
                let config_path = config_dir()?.join(CONFIG_FILE);
                let env_url = std::env::var("DSH_URL").ok();
                let restart = setup_deepseek_at(&config_path, endpoint, env_url).await?;
                restart_if_running(
                    restart,
                    yes,
                    "DeepSeek was already on for this endpoint; nothing to restart",
                )?;
            }
            SetupAgent::Opencode => setup_opencode()?,
        },
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

/// Probe the T3 server, obtain a bearer, store it where the gateway reads
/// it, and enable T3 in `config_path`. The bearer comes from `token` (a
/// pairing code, exchanged here) or else from `t3 auth session issue` run
/// through `t3_bin`. `Ok(None)` when there is neither; otherwise whether the
/// running gateway needs a restart to see the change.
pub(crate) async fn setup_t3_at(
    config_path: &Path,
    state_dir: &Path,
    url: Option<String>,
    token: Option<String>,
    t3_bin: Option<PathBuf>,
    base_dir: Option<&Path>,
) -> anyhow::Result<Option<bool>> {
    let config = load_config(Some(config_path.to_string_lossy().into_owned()))?;
    let raw = url
        .or_else(|| config.t3.url.clone())
        .unwrap_or_else(|| DEFAULT_T3_URL.to_string());
    let endpoint = T3Endpoint::new(raw, T3Credential::None);
    let http = reqwest::Client::new();
    let descriptor = endpoint.describe(&http).await.map_err(|err| {
        anyhow!(
            "No T3 server at {} ({err}): open T3 Code, or run `t3 service install`",
            endpoint.url
        )
    })?;
    println!(
        "==> T3 Code {} answers at {}",
        descriptor.server_version, endpoint.url
    );

    let bearer = match (token, t3_bin) {
        (Some(code), _) => {
            let grant = endpoint
                .exchange_pairing(&http, code.trim(), CLIENT_LABEL)
                .await
                .map_err(|err| anyhow!("T3 did not accept the pairing code: {err}"))?;
            println!("==> Exchanged the pairing code for a bearer");
            grant.token
        }
        (None, Some(bin)) => {
            let bearer = issue_t3_bearer(&bin, base_dir)?;
            println!("==> Issued a bearer with `t3 auth session issue`");
            bearer
        }
        (None, None) => return Ok(None),
    };
    let session = endpoint
        .session_state(&http, &bearer)
        .await
        .map_err(|err| anyhow!("could not check the bearer with T3: {err}"))?;
    anyhow::ensure!(
        session.get("authenticated") == Some(&serde_json::Value::Bool(true)),
        "T3 at {} does not accept the bearer; if it keeps its data elsewhere, pass --base-dir",
        endpoint.url
    );

    write_t3_credential_at(
        state_dir,
        &T3StoredCredential {
            url: endpoint.url.clone(),
            token: bearer,
            saved_at_ms: now_unix_ms() as u64,
        },
    )?;
    println!("==> Stored the credential in {}", state_dir.display());
    if config.t3.token.is_some() {
        println!("    note: `t3.token` in config.json wins over it; remove `t3.token` to use it");
    }

    let before = apply_config_edit(config_path, |config| {
        enable_t3(&mut config.t3, &endpoint.url)
    })?;
    println!(
        "==> Enabled T3 at {} in {}",
        endpoint.url,
        config_path.display()
    );
    Ok(Some(t3_needs_restart(&before.t3, &endpoint.url)))
}

/// A bearer minted locally by the T3 CLI, without a pairing round trip.
fn issue_t3_bearer(t3: &Path, base_dir: Option<&Path>) -> anyhow::Result<String> {
    let mut command = ProcessCommand::new(t3);
    command.args(["auth", "session", "issue", "--token-only"]);
    // A long TTL: the gateway keeps this bearer and cannot renew it.
    command.args([
        "--label",
        CLIENT_LABEL,
        "--ttl",
        "30d",
        "--log-level",
        "none",
    ]);
    if let Some(dir) = base_dir {
        command.arg("--base-dir").arg(dir);
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {}", t3.display()))?;
    anyhow::ensure!(
        output.status.success(),
        "`t3 auth session issue` failed: {}\nrun `t3 pair` and re-run with `--token <code>`",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_issued_token(&String::from_utf8_lossy(&output.stdout))
        .context("`t3 auth session issue` printed no token")
}

/// The token in `t3 auth ... --token-only` output, or in its `--json` form.
pub(crate) fn parse_issued_token(stdout: &str) -> Option<String> {
    let text = stdout.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return ["token", "accessToken", "access_token", "credential"]
            .iter()
            .find_map(|key| value.get(key)?.as_str())
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
    }
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.contains(char::is_whitespace))
        .map(str::to_owned)
}

/// Find a healthy DeepSeek Harness and enable it in `config_path`; returns
/// whether the running gateway needs a restart to see the change.
pub(crate) async fn setup_deepseek_at(
    config_path: &Path,
    endpoint: Option<String>,
    env_url: Option<String>,
) -> anyhow::Result<bool> {
    let config = load_config(Some(config_path.to_string_lossy().into_owned()))?;
    match dsh_credentials_path() {
        Some(path) if path.exists() => {}
        path => println!(
            "    warning: no {}; the Harness will refuse the gateway until it has created it",
            path.map_or("~/.dsh/.credentials.yaml".into(), |p| p
                .display()
                .to_string())
        ),
    }
    let candidates = deepseek_candidates(endpoint, config.deepseek.endpoint.clone(), env_url);
    let secret = config.deepseek.secret.clone().or_else(load_local_secret);
    let http = reqwest::Client::new();
    let mut found = None;
    for url in &candidates {
        let probe = DeepseekEndpoint::new(url, config.deepseek.token.clone(), secret.clone());
        if probe.probe_healthy(&http).await {
            found = Some(probe.url);
            break;
        }
    }
    let Some(url) = found else {
        anyhow::bail!(
            "No DeepSeek Harness answers at {}. Start DeepSeek Harness first: `{DSH_START}`",
            candidates.join(", ")
        );
    };
    println!("==> DeepSeek Harness answers at {url}");
    let before = apply_config_edit(config_path, |config| {
        enable_deepseek(&mut config.deepseek, &url)
    })?;
    println!("==> Enabled DeepSeek at {url} in {}", config_path.display());
    Ok(deepseek_needs_restart(&before.deepseek, &url))
}

/// Where to look for the Harness: the flag, else the configured endpoint,
/// else `DSH_URL`, else its two default local ports.
pub(crate) fn deepseek_candidates(
    endpoint: Option<String>,
    configured: Option<String>,
    env_url: Option<String>,
) -> Vec<String> {
    match endpoint.or(configured).or(env_url) {
        Some(url) => vec![url],
        None => vec![
            "http://127.0.0.1:3080".to_string(),
            "http://127.0.0.1:19387".to_string(),
        ],
    }
}

pub(crate) fn enable_deepseek(deepseek: &mut DeepseekConfig, url: &str) {
    deepseek.enabled = true;
    deepseek.endpoint = Some(url.to_string());
}

/// The DeepSeek settings are read once at start, and a configured endpoint
/// already turns the agent on, so only a new endpoint needs a restart.
pub(crate) fn deepseek_needs_restart(before: &DeepseekConfig, url: &str) -> bool {
    before.endpoint.as_deref() != Some(url)
}

/// The Harness credentials file the gateway signs its cookie with, as
/// `load_local_secret` finds it.
fn dsh_credentials_path() -> Option<PathBuf> {
    let home = std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".dsh")))?;
    Some(home.join(".credentials.yaml"))
}

fn setup_opencode() -> anyhow::Result<()> {
    let config = load_config(None)?;
    println!(
        "OpenCode is managed by the gateway: it adopts a running `opencode service`, or \
         starts one when `opencode.autostart` is on. Nothing to set up."
    );
    match resolve_binary(config.opencode.binary.as_deref()) {
        Ok(path) => {
            println!("binary:  {}", path.display());
            let version = binary_version(&path);
            println!("version: {}", version.as_deref().unwrap_or("unknown"));
        }
        Err(err) => println!("{err:#}"),
    }
    Ok(())
}

/// Enable T3 at `url`, dropping any pairing code: the bearer replaces it.
pub(crate) fn enable_t3(t3: &mut T3Config, url: &str) {
    t3.enabled = true;
    t3.url = Some(url.to_string());
    t3.pairing_token = None;
}

/// A running gateway re-reads the stored bearer each round, but only for
/// the URL it started with, and only if T3 was on at all.
pub(crate) fn t3_needs_restart(before: &T3Config, url: &str) -> bool {
    !before.wanted() || before.url.as_deref().unwrap_or(DEFAULT_T3_URL) != url
}

/// Load `path`, apply `edit`, save it atomically; returns the config as it
/// was before.
pub(crate) fn apply_config_edit(
    path: &Path,
    edit: impl FnOnce(&mut Config),
) -> anyhow::Result<Config> {
    let mut config = load_config(Some(path.to_string_lossy().into_owned()))?;
    let before = config.clone();
    edit(&mut config);
    write_config(path, &config)?;
    Ok(before)
}

/// Restart the gateway's user service when it runs and `needed`, asking
/// first unless `yes`; otherwise say how the change takes effect.
fn restart_if_running(needed: bool, yes: bool, unneeded_note: &str) -> anyhow::Result<()> {
    if !needed {
        println!("==> {unneeded_note}");
        return Ok(());
    }
    let unit = format!("{SERVICE_LABEL}.service");
    let active = which("systemctl").is_some()
        && ProcessCommand::new("systemctl")
            .args(["--user", "is-active", "--quiet", &unit])
            .status()
            .is_ok_and(|status| status.success());
    if !active {
        println!("==> Restart the gateway to apply");
        return Ok(());
    }
    if !yes && !confirm(&format!("Restart {unit} now? [y/N] "))? {
        println!("==> Not restarted; restart the gateway to apply");
        return Ok(());
    }
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "restart", &unit])
        .status()?;
    anyhow::ensure!(status.success(), "`systemctl --user restart {unit}` failed");
    println!("==> Restarted {unit}");
    Ok(())
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `command` as `PATH` resolves it.
// TODO(windows): honour PATHEXT so `t3` finds `t3.cmd` / `t3.exe`.
pub(crate) fn which(command: &str) -> Option<PathBuf> {
    crate::tasks::find_on_path(command)
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

    #[test]
    fn issued_tokens_parse_from_plain_and_json_output() {
        assert_eq!(parse_issued_token("abc.def\n").as_deref(), Some("abc.def"));
        assert_eq!(
            parse_issued_token("Issued a token for muqun-gateway\nabc.def\n").as_deref(),
            Some("abc.def")
        );
        assert_eq!(
            parse_issued_token(r#"{"token":"tok","expiresAt":"x"}"#).as_deref(),
            Some("tok")
        );
        assert_eq!(
            parse_issued_token(r#"{"accessToken":"tok2"}"#).as_deref(),
            Some("tok2")
        );
        assert_eq!(parse_issued_token(r#"{"token":""}"#), None);
        assert_eq!(parse_issued_token("  \n"), None);
    }

    #[test]
    fn t3_restart_is_needed_only_when_the_running_t3_target_changes() {
        let url = "http://127.0.0.1:3773";
        assert!(t3_needs_restart(&T3Config::default(), url));
        let on = T3Config {
            enabled: true,
            ..Default::default()
        };
        assert!(!t3_needs_restart(&on, url), "default URL, already on");
        let elsewhere = T3Config {
            url: Some("http://h:1".into()),
            ..Default::default()
        };
        assert!(t3_needs_restart(&elsewhere, url));
        assert!(!t3_needs_restart(&elsewhere, "http://h:1"));
    }

    fn temp_config(t3: T3Config) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agent-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE);
        let mut config = crate::test_support::test_config("admin");
        config.label = "keep me".into();
        config.opencode.binary = Some("/opt/opencode".into());
        config.t3 = t3;
        write_config(&path, &config).unwrap();
        (dir, path)
    }

    fn reload(path: &Path) -> Config {
        load_config(Some(path.to_string_lossy().into_owned())).unwrap()
    }

    #[test]
    fn enabling_t3_drops_the_pairing_code_and_keeps_the_rest() {
        let (dir, path) = temp_config(T3Config {
            pairing_token: Some("spent".into()),
            runtime_mode: Some("full-access".into()),
            ..Default::default()
        });
        let before = apply_config_edit(&path, |config| {
            enable_t3(&mut config.t3, "http://127.0.0.1:3773")
        })
        .unwrap();
        assert_eq!(before.t3.pairing_token.as_deref(), Some("spent"));
        let after = reload(&path);
        assert!(after.t3.enabled);
        assert_eq!(after.t3.url.as_deref(), Some("http://127.0.0.1:3773"));
        assert_eq!(after.t3.pairing_token, None);
        assert_eq!(after.t3.runtime_mode.as_deref(), Some("full-access"));
        assert_eq!(after.label, "keep me");
        assert_eq!(after.opencode.binary.as_deref(), Some("/opt/opencode"));
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("pairing_token"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// A T3 server with the three routes setup touches: the descriptor, the
    /// pairing exchange (code `once` mints `minted`) and the session check.
    async fn fake_t3() -> String {
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        let app = axum::Router::new()
            .route(
                "/.well-known/t3/environment",
                get(|| async {
                    r#"{"environmentId":"env","label":"t3","serverVersion":"0.0.44"}"#
                }),
            )
            .route(
                "/oauth/token",
                post(|body: String| async move {
                    if body.contains("subject_token=once") {
                        (
                            StatusCode::OK,
                            r#"{"access_token":"minted","token_type":"Bearer","expires_in":3600,"scope":"s"}"#,
                        )
                    } else {
                        (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_grant"}"#)
                    }
                }),
            )
            .route(
                "/api/auth/session",
                get(|headers: axum::http::HeaderMap| async move {
                    let ok = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        == Some("Bearer minted");
                    format!(r#"{{"authenticated":{ok}}}"#)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    #[tokio::test]
    async fn setup_t3_exchanges_a_pairing_code_stores_the_bearer_and_enables_t3() {
        let url = fake_t3().await;
        let (dir, path) = temp_config(T3Config {
            pairing_token: Some("old".into()),
            ..Default::default()
        });
        let state = dir.join("state");
        let setup = |url: String, token: Option<&str>| {
            setup_t3_at(
                &path,
                &state,
                Some(url),
                token.map(str::to_owned),
                None,
                None,
            )
        };

        // A refused code changes nothing.
        assert!(setup(url.clone(), Some("nope")).await.is_err());
        assert!(crate::read_t3_credential_at(&state).unwrap().is_none());
        assert_eq!(reload(&path).t3.pairing_token.as_deref(), Some("old"));

        // No code and no `t3` binary: nothing to get a bearer from.
        assert_eq!(setup(url.clone(), None).await.unwrap(), None);

        let restart = setup(format!("{url}/"), Some("once")).await.unwrap();
        assert_eq!(restart, Some(true), "T3 was off before");
        let stored = crate::read_t3_credential_at(&state).unwrap().unwrap();
        assert_eq!(
            (stored.url.as_str(), stored.token.as_str()),
            (url.as_str(), "minted")
        );
        let config = reload(&path);
        assert!(config.t3.enabled);
        assert_eq!(config.t3.url.as_deref(), Some(url.as_str()));
        assert_eq!(config.t3.pairing_token, None);

        // Again, with T3 already on for that URL: no restart.
        let again = setup(url.clone(), Some("once")).await.unwrap();
        assert_eq!(again, Some(false));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn setup_t3_fails_clearly_when_no_server_answers() {
        let (dir, path) = temp_config(T3Config::default());
        let err = setup_t3_at(
            &path,
            &dir.join("state"),
            Some("http://127.0.0.1:9".into()),
            Some("once".into()),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("No T3 server at http://127.0.0.1:9"),
            "{err}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn deepseek_candidates_follow_flag_config_env_then_ports() {
        let some = |v: &str| Some(v.to_string());
        assert_eq!(
            deepseek_candidates(some("http://a"), some("http://b"), some("http://c")),
            ["http://a"]
        );
        assert_eq!(
            deepseek_candidates(None, some("http://b"), some("http://c")),
            ["http://b"]
        );
        assert_eq!(
            deepseek_candidates(None, None, some("http://c")),
            ["http://c"]
        );
        assert_eq!(
            deepseek_candidates(None, None, None),
            ["http://127.0.0.1:3080", "http://127.0.0.1:19387"]
        );
    }

    #[test]
    fn enabling_deepseek_keeps_its_credentials_and_the_rest() {
        let (dir, path) = temp_config(T3Config::default());
        apply_config_edit(&path, |config| {
            config.deepseek.token = Some("tok".into());
        })
        .unwrap();
        let before = apply_config_edit(&path, |config| {
            enable_deepseek(&mut config.deepseek, "http://127.0.0.1:3080")
        })
        .unwrap();
        assert!(deepseek_needs_restart(
            &before.deepseek,
            "http://127.0.0.1:3080"
        ));
        let after = reload(&path);
        assert!(after.deepseek.enabled);
        assert_eq!(
            after.deepseek.endpoint.as_deref(),
            Some("http://127.0.0.1:3080")
        );
        assert_eq!(after.deepseek.token.as_deref(), Some("tok"));
        assert_eq!(after.label, "keep me");
        assert!(!deepseek_needs_restart(
            &after.deepseek,
            "http://127.0.0.1:3080"
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn setup_deepseek_fails_clearly_when_no_harness_answers() {
        let (dir, path) = temp_config(T3Config::default());
        let err = setup_deepseek_at(&path, Some("http://127.0.0.1:9".into()), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Start DeepSeek Harness first"),
            "{err}"
        );
        assert!(!reload(&path).deepseek.enabled, "config untouched");
        std::fs::remove_dir_all(dir).ok();
    }
}
