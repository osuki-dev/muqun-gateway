//! The T3 Code side of the runtime: its `config.json` block, the credential
//! it holds for the server, and how it is attached, supervised and
//! reported in discovery.
//!
//! T3 is opt-in like DeepSeek, and never looked for: nothing is attached
//! unless `t3.enabled` is true or `t3.url` is set, and then only the URL the
//! owner gave (or T3's default loopback port when they gave none) is tried.
//!
//! The credential is a one-time pairing token from `t3 pair`, which the
//! gateway exchanges once for a bearer and keeps in `t3-credential.json`
//! under the state directory. The pairing token is forgotten as soon as it
//! is spent and is never written back to `config.json`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{probe_client, AgentOrigin, AgentRuntime};
use crate::agents::adapters::t3::{self, T3Credential, T3Endpoint};
use crate::agents::manager::AgentManager;
use crate::platform::store::{read_t3_credential_at, write_t3_credential_at, T3StoredCredential};

/// Where a T3 Code server listens when the owner names no URL.
pub const DEFAULT_T3_URL: &str = "http://127.0.0.1:3773";
/// How the gateway names itself to T3 when it pairs.
const PAIRING_CLIENT_LABEL: &str = "muqun-gateway";

/// `t3` in `config.json`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct T3Config {
    /// Attach to a T3 Code server at `url`, or at the default loopback port.
    #[serde(default)]
    pub enabled: bool,
    /// The server, `http(s)://host:port`. Setting it also enables T3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A bearer the owner already holds. Takes precedence over a stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// A one-time pairing token from `t3 pair`, exchanged once for a bearer
    /// that is then kept in the state directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_token: Option<String>,
    /// The T3 runtime mode new threads get: `full-access` (T3's default),
    /// `approval-required`, `auto-accept-edits` or `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_mode: Option<String>,
}

/// `Debug` never prints a token: `Config` derives `Debug` and is logged.
impl std::fmt::Debug for T3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        f.debug_struct("T3Config")
            .field("enabled", &self.enabled)
            .field("url", &self.url)
            .field("token", &redact(&self.token))
            .field("pairing_token", &redact(&self.pairing_token))
            .field("runtime_mode", &self.runtime_mode)
            .finish()
    }
}

impl T3Config {
    /// Whether the gateway attaches to T3 at all. Decided by `config.json`
    /// alone: the environment can supply values, never switch T3 on.
    pub fn wanted(&self) -> bool {
        self.enabled || self.url.is_some()
    }

    /// `T3_URL`, `T3_TOKEN` and `T3_PAIRING_TOKEN` fill what the file
    /// leaves unset, and nothing it sets.
    pub fn with_env_fallback(mut self, env: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| {
            env(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        if self.url.is_none() {
            self.url = get("T3_URL");
        }
        if self.token.is_none() {
            self.token = get("T3_TOKEN");
        }
        if self.pairing_token.is_none() {
            self.pairing_token = get("T3_PAIRING_TOKEN");
        }
        self
    }
}

/// What the runtime keeps for T3 between supervision passes.
pub(super) struct T3State {
    /// Whether `config.json` asked for T3; see [`T3Config::wanted`].
    wanted: bool,
    /// The configuration with the environment fallback applied.
    config: T3Config,
    /// The pairing token until it has been spent.
    pairing: Mutex<Option<String>>,
    /// Where the bearer is kept; `None` keeps it in memory only.
    state_dir: Option<PathBuf>,
    /// A bearer obtained this run, when it could not be stored.
    exchanged: Mutex<Option<String>>,
    /// "Nothing to authenticate with" is said once, not every poll.
    warned_no_credential: AtomicBool,
}

impl T3State {
    pub(super) fn new(file_config: T3Config, state_dir: Option<PathBuf>) -> Self {
        Self::with_env(file_config, state_dir, |key| std::env::var(key).ok())
    }

    pub(super) fn with_env(
        file_config: T3Config,
        state_dir: Option<PathBuf>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let wanted = file_config.wanted();
        let config = file_config.with_env_fallback(env);
        Self {
            wanted,
            pairing: Mutex::new(config.pairing_token.clone()),
            config,
            state_dir,
            exchanged: Mutex::new(None),
            warned_no_credential: AtomicBool::new(false),
        }
    }

    pub(super) fn wanted(&self) -> bool {
        self.wanted
    }

    /// The server URL, as the owner gave it or T3's default.
    pub(super) fn url(&self) -> String {
        self.config
            .url
            .clone()
            .unwrap_or_else(|| DEFAULT_T3_URL.to_string())
    }

    /// The endpoint the owner configured, without a credential.
    pub(super) fn endpoint(&self) -> T3Endpoint {
        T3Endpoint::new(self.url(), T3Credential::None)
    }

    fn stored_bearer(&self, url: &str) -> Option<String> {
        let dir = self.state_dir.as_ref()?;
        match read_t3_credential_at(dir) {
            Ok(Some(stored)) if stored.url == url => Some(stored.token),
            Ok(Some(_)) => {
                tracing::info!("the stored T3 credential belongs to another server; ignoring it");
                None
            }
            Ok(None) => None,
            Err(err) => {
                tracing::warn!("could not read the stored T3 credential: {err:#}");
                None
            }
        }
    }

    fn persist(&self, url: &str, token: &str) {
        let Some(dir) = self.state_dir.as_ref() else {
            return;
        };
        let stored = T3StoredCredential {
            url: url.to_string(),
            token: token.to_string(),
            saved_at_ms: crate::platform::store::now_unix_ms() as u64,
        };
        match write_t3_credential_at(dir, &stored) {
            Ok(()) => tracing::info!("stored the T3 credential"),
            Err(err) => tracing::warn!(
                "could not store the T3 credential ({err:#}); it is kept for this run only, \
                 and a restart will need a new pairing token"
            ),
        }
    }

    /// The credential to attach with: the configured bearer, else a stored
    /// one the server still accepts, else the pairing token exchanged now
    /// (and the bearer stored), else nothing.
    pub(super) async fn credential(
        &self,
        endpoint: &T3Endpoint,
        http: &reqwest::Client,
    ) -> Option<T3Credential> {
        if let Some(token) = self.config.token.clone() {
            return Some(T3Credential::Bearer(token));
        }
        let remembered = match self.exchanged.lock().await.clone() {
            Some(token) => Some(token),
            None => self.stored_bearer(&endpoint.url),
        };
        if let Some(token) = remembered {
            match endpoint.session_state(http, &token).await {
                Ok(state) if state.get("authenticated").and_then(|v| v.as_bool()) == Some(true) => {
                    return Some(T3Credential::Bearer(token));
                }
                Ok(_) => tracing::warn!(
                    "the T3 server no longer accepts the stored credential; pair again \
                     with a new t3.pairing_token"
                ),
                // Unreachable is not rejected: keep the bearer for the next try.
                Err(_) => return Some(T3Credential::Bearer(token)),
            }
        }
        let pairing = self.pairing.lock().await.take()?;
        match endpoint
            .exchange_pairing(http, &pairing, PAIRING_CLIENT_LABEL)
            .await
        {
            Ok(grant) => {
                tracing::info!(
                    expires_in_secs = grant.expires_in_secs,
                    "paired with the T3 server"
                );
                *self.exchanged.lock().await = Some(grant.token.clone());
                self.persist(&endpoint.url, &grant.token);
                Some(T3Credential::Bearer(grant.token))
            }
            Err(err) => {
                // A network failure leaves the token unspent; anything else
                // means the server has refused it, and asking again every few
                // seconds with the same token would only fill the log.
                if matches!(err, crate::agents::ports::AgentError::Network(_)) {
                    *self.pairing.lock().await = Some(pairing);
                } else {
                    tracing::warn!(
                        %err,
                        "the T3 pairing token was refused; run `t3 pair` for a new one"
                    );
                }
                None
            }
        }
    }

    fn runtime_mode(&self) -> Option<&str> {
        self.config.runtime_mode.as_deref()
    }
}

/// The feature flags the App reads for the `t3` agent.
pub(super) fn t3_features() -> crate::discovery::AgentFeatures {
    crate::discovery::AgentFeatures {
        streaming: true,
        reasoning_effort: false,
        model_selection: true,
        tool_approvals: true,
        worktrees: false,
        revert: true,
        inbox: false,
        // The driver answers `Unsupported` for a mode, attachments, skills,
        // commands, compaction and shells.
        modes: false,
        skills: false,
        slash_commands: false,
        compaction: false,
        background_shells: false,
        attachments: false,
        extra: std::collections::BTreeMap::new(),
    }
}

impl AgentRuntime {
    /// One supervision pass for T3. Returns whether it is attached and well.
    pub(super) async fn supervise_t3(&self) -> bool {
        if !self.t3.wanted() {
            return false;
        }
        if self.keep_if_healthy(t3::KIND).await {
            return true;
        }
        let http = probe_client();
        let endpoint = self.t3.endpoint();
        let descriptor = match endpoint.describe(&http).await {
            Ok(descriptor) => descriptor,
            Err(err) => {
                tracing::debug!(url = %endpoint.url, %err, "no T3 server answering");
                return false;
            }
        };
        if descriptor.protocol_version() > t3::ORCHESTRATION_PROTOCOL_VERSION {
            tracing::warn!(
                version = descriptor.protocol_version(),
                "the T3 server speaks a newer orchestration protocol than this gateway"
            );
        }
        let Some(credential) = self.t3.credential(&endpoint, &http).await else {
            if !self.t3.warned_no_credential.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    url = %endpoint.url,
                    "a T3 server is answering but the gateway has no credential for it; \
                     set t3.pairing_token to the token `t3 pair` prints"
                );
            }
            return false;
        };
        let endpoint = T3Endpoint::new(endpoint.url, credential);
        let manager = AgentManager::connect_t3(
            endpoint,
            self.t3.runtime_mode(),
            Some(descriptor.server_version.clone()),
            self.events_tx.clone(),
        );
        self.install(t3::KIND, manager, AgentOrigin::Adopted).await;
        tracing::info!(
            url = %self.t3.url(),
            version = %descriptor.server_version,
            "adopted the running T3 Code server"
        );
        true
    }

    /// The `t3` entry of `planes.agents.agents[]`.
    pub(super) async fn t3_discovery(&self) -> crate::discovery::AgentDiscoveryInfo {
        use crate::discovery::{AgentAvailability, AgentDiscoveryInfo};
        let mut info = AgentDiscoveryInfo {
            id: t3::KIND.to_string(),
            name: "T3 Code".to_string(),
            kind: t3::KIND.to_string(),
            status: AgentAvailability::Disabled,
            enabled: self.t3.wanted(),
            endpoint: self.t3.config.url.clone(),
            version: None,
            models: Vec::new(),
            modes: Vec::new(),
            features: t3_features(),
        };
        if !self.t3.wanted() {
            return info;
        }
        info.endpoint = Some(self.t3.url());
        if let Some(manager) = self.manager_for_agent(t3::KIND).await {
            info.status = AgentAvailability::Connected;
            info.endpoint = Some(manager.endpoint_url().to_string());
            info.version = manager.version();
            (info.models, info.modes) = catalog_entry(manager.agent().as_ref()).await;
            return info;
        }
        // Not attached: the descriptor is public, so reachability and the
        // version can be told without a credential; the models cannot.
        match self.t3.endpoint().describe(&probe_client()).await {
            Ok(descriptor) => {
                info.status = AgentAvailability::Unconfigured;
                info.version = Some(descriptor.server_version);
            }
            Err(_) => info.status = AgentAvailability::Offline,
        }
        info
    }
}

/// An agent's models and modes for discovery; empty when the catalog could
/// not be read.
pub(super) async fn catalog_entry(
    agent: &dyn crate::agents::ports::AgentPort,
) -> (
    Vec<crate::discovery::AgentModelInfo>,
    Vec<crate::discovery::AgentModeInfo>,
) {
    match agent.get_catalog(None).await {
        Ok(catalog) => (
            catalog
                .models
                .iter()
                .map(crate::discovery::model_info_to_agent_model)
                .collect(),
            catalog
                .modes
                .iter()
                .map(crate::discovery::mode_info_to_agent_mode)
                .collect(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn t3_features_declare_no_agent_specific_extras() {
        let v = serde_json::to_value(super::t3_features()).unwrap();
        for key in [
            "modes",
            "skills",
            "slashCommands",
            "compaction",
            "backgroundShells",
            "attachments",
        ] {
            assert_eq!(v[key], false, "{key}");
        }
    }

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn the_t3_block_defaults_to_off_and_parses_every_key() {
        let empty: T3Config = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, T3Config::default());
        assert!(!empty.wanted());

        let full: T3Config = serde_json::from_str(
            r#"{"enabled": true, "url": "http://127.0.0.1:3773", "token": "b",
                "pairing_token": "p", "runtime_mode": "approval-required"}"#,
        )
        .unwrap();
        assert!(full.enabled);
        assert_eq!(full.url.as_deref(), Some("http://127.0.0.1:3773"));
        assert_eq!(full.token.as_deref(), Some("b"));
        assert_eq!(full.pairing_token.as_deref(), Some("p"));
        assert_eq!(full.runtime_mode.as_deref(), Some("approval-required"));

        let url_only: T3Config = serde_json::from_str(r#"{"url": "http://h:1"}"#).unwrap();
        assert!(url_only.wanted(), "a URL alone enables T3");
        // Unset keys are not written back.
        assert_eq!(
            serde_json::to_string(&url_only).unwrap(),
            r#"{"enabled":false,"url":"http://h:1"}"#
        );
    }

    #[test]
    fn debug_never_prints_either_token() {
        let config = T3Config {
            enabled: true,
            url: Some("http://127.0.0.1:3773".into()),
            token: Some("bearer-secret".into()),
            pairing_token: Some("pairing-secret".into()),
            runtime_mode: None,
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("bearer-secret"));
        assert!(!shown.contains("pairing-secret"));
        assert!(shown.contains("<redacted>"));
        assert!(shown.contains("127.0.0.1:3773"));
    }

    #[test]
    fn the_environment_fills_gaps_and_never_enables() {
        let env = env_of(&[
            ("T3_URL", "http://env:1"),
            ("T3_TOKEN", "env-bearer"),
            ("T3_PAIRING_TOKEN", "env-pairing"),
        ]);
        let off = T3State::with_env(T3Config::default(), None, &env);
        assert!(!off.wanted(), "the environment does not switch T3 on");

        let on = T3State::with_env(
            T3Config {
                enabled: true,
                ..Default::default()
            },
            None,
            &env,
        );
        assert!(on.wanted());
        assert_eq!(on.url(), "http://env:1");
        assert_eq!(on.config.token.as_deref(), Some("env-bearer"));

        let configured = T3State::with_env(
            T3Config {
                url: Some("http://file:2".into()),
                pairing_token: Some("file-pairing".into()),
                ..Default::default()
            },
            None,
            &env,
        );
        assert_eq!(configured.url(), "http://file:2", "the file wins");
        assert_eq!(
            configured.config.pairing_token.as_deref(),
            Some("file-pairing")
        );

        let bare = T3State::with_env(
            T3Config {
                enabled: true,
                ..Default::default()
            },
            None,
            env_of(&[]),
        );
        assert_eq!(bare.url(), DEFAULT_T3_URL);
    }

    #[tokio::test]
    async fn a_stored_bearer_is_offered_only_to_its_own_server() {
        let dir = std::env::temp_dir().join(format!("t3-rt-{}", uuid::Uuid::new_v4()));
        write_t3_credential_at(
            &dir,
            &T3StoredCredential {
                url: "http://127.0.0.1:9".into(),
                token: "stored".into(),
                saved_at_ms: 0,
            },
        )
        .unwrap();
        let state = T3State::with_env(
            T3Config {
                enabled: true,
                url: Some("http://127.0.0.1:9".into()),
                ..Default::default()
            },
            Some(dir.clone()),
            env_of(&[]),
        );
        assert_eq!(
            state.stored_bearer("http://127.0.0.1:9").as_deref(),
            Some("stored")
        );
        assert_eq!(state.stored_bearer("http://127.0.0.1:10"), None);
        // Port 9 refuses: an unreachable server does not make a stored
        // bearer invalid, so it is still what the gateway attaches with.
        let credential = state.credential(&state.endpoint(), &probe_client()).await;
        assert_eq!(credential, Some(T3Credential::Bearer("stored".into())));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stand-in for the two T3 routes pairing touches.
    async fn fake_t3() -> String {
        use axum::routing::{get, post};
        let app = axum::Router::new()
            .route(
                "/oauth/token",
                post(|body: String| async move {
                    if body.contains("subject_token=once") {
                        (
                            axum::http::StatusCode::OK,
                            r#"{"access_token":"minted","token_type":"Bearer","expires_in":3600,"scope":"orchestration:read"}"#,
                        )
                    } else {
                        (axum::http::StatusCode::UNAUTHORIZED, r#"{"error":"invalid_grant"}"#)
                    }
                }),
            )
            .route(
                "/api/auth/session",
                get(|headers: axum::http::HeaderMap| async move {
                    let ok = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
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
    async fn a_pairing_token_is_spent_once_and_the_bearer_stored() {
        let url = fake_t3().await;
        let dir = std::env::temp_dir().join(format!("t3-pair-{}", uuid::Uuid::new_v4()));
        let config = T3Config {
            url: Some(url.clone()),
            pairing_token: Some("once".into()),
            ..Default::default()
        };
        let state = T3State::with_env(config.clone(), Some(dir.clone()), env_of(&[]));
        let http = probe_client();
        assert_eq!(
            state.credential(&state.endpoint(), &http).await,
            Some(T3Credential::Bearer("minted".into()))
        );
        assert!(
            state.pairing.lock().await.is_none(),
            "the pairing token is forgotten"
        );
        let stored = read_t3_credential_at(&dir).unwrap().unwrap();
        assert_eq!(
            (stored.url.as_str(), stored.token.as_str()),
            (url.as_str(), "minted")
        );

        // A restart with the same config.json: the spent pairing token is
        // still in the file, but the stored bearer is used and it is not
        // exchanged again.
        let restarted = T3State::with_env(config, Some(dir.clone()), env_of(&[]));
        assert_eq!(
            restarted.credential(&restarted.endpoint(), &http).await,
            Some(T3Credential::Bearer("minted".into()))
        );
        assert_eq!(restarted.pairing.lock().await.as_deref(), Some("once"));

        // A refused token is dropped rather than retried every poll.
        let refused = T3State::with_env(
            T3Config {
                url: Some(url),
                pairing_token: Some("stale".into()),
                ..Default::default()
            },
            None,
            env_of(&[]),
        );
        assert_eq!(refused.credential(&refused.endpoint(), &http).await, None);
        assert!(refused.pairing.lock().await.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unreachable_server_leaves_the_pairing_token_unspent() {
        let state = T3State::with_env(
            T3Config {
                url: Some("http://127.0.0.1:9".into()),
                pairing_token: Some("once".into()),
                ..Default::default()
            },
            None,
            env_of(&[]),
        );
        assert_eq!(
            state.credential(&state.endpoint(), &probe_client()).await,
            None
        );
        assert_eq!(
            state.pairing.lock().await.as_deref(),
            Some("once"),
            "a network failure did not consume it"
        );
    }
}
