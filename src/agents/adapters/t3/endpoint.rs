//! Where a T3 Code server is and how the gateway proves itself to it.
//!
//! The auth flow, as observed against `t3 serve` 0.0.42:
//!
//! 1. `GET /.well-known/t3/environment` (unauthenticated) returns the
//!    [`EnvironmentDescriptor`]: environment id, label, server version and
//!    capability flags.
//! 2. A one-time pairing credential (`t3 pair`, or the `#token=` fragment of
//!    a pairing link) is exchanged at `POST /oauth/token` (form-encoded
//!    RFC 8693 token exchange) for a bearer access token. The pairing
//!    credential is consumed by the exchange.
//! 3. `POST /api/auth/websocket-ticket` with the bearer mints a five-minute
//!    ticket; the socket is opened at `GET /ws?wsTicket=<ticket>`. Long-lived
//!    tokens never travel in the socket URL.
//!
//! Nothing here logs a credential.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use super::MAX_PAYLOAD_BYTES;
use crate::agents::ports::agent::AgentError;

pub const WELL_KNOWN_PATH: &str = "/.well-known/t3/environment";
pub const TOKEN_PATH: &str = "/oauth/token";
pub const TICKET_PATH: &str = "/api/auth/websocket-ticket";
pub const SESSION_PATH: &str = "/api/auth/session";
pub const WS_PATH: &str = "/ws";
pub const DEFAULT_PORT: u16 = 3773;

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const SUBJECT_TOKEN_TYPE: &str = "urn:t3:params:oauth:token-type:environment-bootstrap";
const REQUESTED_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";

/// The credential the gateway holds for a server.
///
/// A pairing credential is single-use: the first successful exchange turns it
/// into a bearer, which the runtime persists. `None` is accepted so a
/// server in `unsafe-no-auth` mode can still be probed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum T3Credential {
    Bearer(String),
    Pairing(String),
    None,
}

impl T3Credential {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// `ExecutionEnvironmentDescriptor` from `/.well-known/t3/environment`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EnvironmentDescriptor {
    #[serde(rename = "environmentId")]
    pub environment_id: String,
    pub label: String,
    #[serde(rename = "serverVersion")]
    pub server_version: String,
    /// Absent on the wire means protocol 1.
    #[serde(rename = "orchestrationProtocolVersion")]
    pub orchestration_protocol_version: Option<u64>,
    #[serde(default)]
    pub platform: Value,
    #[serde(default)]
    pub capabilities: Value,
}

impl EnvironmentDescriptor {
    pub fn protocol_version(&self) -> u64 {
        self.orchestration_protocol_version
            .unwrap_or(super::ORCHESTRATION_PROTOCOL_VERSION)
    }

    pub fn capability(&self, name: &str) -> bool {
        self.capabilities
            .get(name)
            .map(|v| v.as_bool().unwrap_or(v.is_object()))
            .unwrap_or(false)
    }
}

/// A bearer grant from the token exchange.
#[derive(Debug, Clone)]
pub struct BearerGrant {
    pub token: String,
    pub expires_in_secs: u64,
    pub scope: String,
}

impl std::fmt::Debug for T3Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("T3Endpoint")
            .field("url", &self.url)
            .field("credential", &self.credential_kind())
            .finish()
    }
}

/// The connection details for one T3 Code server.
#[derive(Clone, PartialEq, Eq)]
pub struct T3Endpoint {
    /// `http(s)://host:port`, no trailing slash.
    pub url: String,
    /// `ws(s)://host:port/ws`, without the ticket.
    pub ws_url: String,
    pub credential: T3Credential,
}

impl T3Endpoint {
    pub fn new(url: impl Into<String>, credential: T3Credential) -> Self {
        let raw: String = url.into();
        let trimmed = raw.trim().trim_end_matches('/').to_string();
        let url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed
        } else {
            format!("http://{trimmed}")
        };
        let ws_url = if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}{WS_PATH}")
        } else {
            let rest = url.trim_start_matches("http://");
            format!("ws://{rest}{WS_PATH}")
        };
        Self {
            url,
            ws_url,
            credential,
        }
    }

    /// `T3_URL` (or `T3CODE_URL`) plus `T3_TOKEN` (bearer) or
    /// `T3_PAIRING_TOKEN`. Used by the live test and as a discovery hint.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("T3_URL")
            .or_else(|_| std::env::var("T3CODE_URL"))
            .ok()
            .filter(|u| !u.trim().is_empty())?;
        let credential = if let Ok(t) = std::env::var("T3_TOKEN") {
            T3Credential::Bearer(t)
        } else if let Ok(t) = std::env::var("T3_PAIRING_TOKEN") {
            T3Credential::Pairing(t)
        } else {
            T3Credential::None
        };
        Some(Self::new(url, credential))
    }

    /// Which kind of credential is held, for logs. Never the value.
    pub fn credential_kind(&self) -> &'static str {
        match self.credential {
            T3Credential::Bearer(_) => "bearer",
            T3Credential::Pairing(_) => "pairing",
            T3Credential::None => "none",
        }
    }

    pub fn ws_url_with_ticket(&self, ticket: &str) -> String {
        format!("{}?wsTicket={}", self.ws_url, url_encode(ticket))
    }

    /// Fetch the environment descriptor. This is the health probe: a server
    /// that answers it with a decodable descriptor is a T3 server.
    pub async fn describe(&self, http: &Client) -> Result<EnvironmentDescriptor, AgentError> {
        let resp = http
            .get(format!("{}{WELL_KNOWN_PATH}", self.url))
            .timeout(Duration::from_millis(2500))
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AgentError::NotAvailable(format!(
                "descriptor returned HTTP {status}"
            )));
        }
        let body = read_bounded(resp, 64 * 1024).await?;
        serde_json::from_slice(&body)
            .map_err(|e| AgentError::Protocol(format!("descriptor: {}", short(&e.to_string()))))
    }

    pub async fn probe_healthy(&self, http: &Client) -> bool {
        self.describe(http).await.is_ok()
    }

    /// Find a server from the environment or on the default loopback port.
    /// A credential from the environment is attached when present; discovery
    /// itself needs none.
    pub async fn discover(http: &Client) -> Option<(Self, EnvironmentDescriptor)> {
        let candidates = match Self::from_env() {
            Some(ep) => vec![ep],
            None => vec![Self::new(
                format!("http://127.0.0.1:{DEFAULT_PORT}"),
                T3Credential::None,
            )],
        };
        for endpoint in candidates {
            match endpoint.describe(http).await {
                Ok(descriptor) => return Some((endpoint, descriptor)),
                Err(e) => {
                    tracing::debug!(url = %endpoint.url, error = %e, "t3 discovery probe failed")
                }
            }
        }
        None
    }

    /// Exchange a one-time pairing credential for a bearer token
    /// (`POST /oauth/token`, form-encoded).
    pub async fn exchange_pairing(
        &self,
        http: &Client,
        pairing: &str,
        client_label: &str,
    ) -> Result<BearerGrant, AgentError> {
        let form = [
            ("grant_type", GRANT_TYPE),
            ("subject_token", pairing),
            ("subject_token_type", SUBJECT_TOKEN_TYPE),
            ("requested_token_type", REQUESTED_TOKEN_TYPE),
            ("client_label", client_label),
            ("client_device_type", "bot"),
        ];
        let resp = http
            .post(format!("{}{TOKEN_PATH}", self.url))
            .timeout(Duration::from_secs(10))
            .form(&form)
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let status = resp.status();
        let body = read_bounded(resp, 64 * 1024).await?;
        if !status.is_success() {
            tracing::warn!(%status, body = %String::from_utf8_lossy(&body[..body.len().min(300)]), "t3 pairing exchange rejected");
            return Err(AgentError::NotAvailable(format!(
                "pairing exchange returned HTTP {status}"
            )));
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default)]
            expires_in: f64,
            #[serde(default)]
            scope: String,
        }
        let parsed: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| AgentError::Protocol(format!("token: {}", short(&e.to_string()))))?;
        Ok(BearerGrant {
            token: parsed.access_token,
            expires_in_secs: parsed.expires_in.max(0.0) as u64,
            scope: parsed.scope,
        })
    }

    /// Mint a WebSocket ticket for the bearer.
    pub async fn websocket_ticket(
        &self,
        http: &Client,
        bearer: &str,
    ) -> Result<String, AgentError> {
        let resp = http
            .post(format!("{}{TICKET_PATH}", self.url))
            .timeout(Duration::from_secs(10))
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let status = resp.status();
        let body = read_bounded(resp, 16 * 1024).await?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(AgentError::NotAvailable(
                "the stored T3 credential was rejected; pair again".into(),
            ));
        }
        if !status.is_success() {
            return Err(AgentError::RequestFailed(format!(
                "websocket ticket returned HTTP {status}"
            )));
        }
        #[derive(Deserialize)]
        struct Ticket {
            ticket: String,
        }
        let parsed: Ticket = serde_json::from_slice(&body)
            .map_err(|e| AgentError::Protocol(format!("ticket: {}", short(&e.to_string()))))?;
        Ok(parsed.ticket)
    }

    /// `GET /api/auth/session`: whether the bearer is still accepted, and its
    /// scopes.
    pub async fn session_state(&self, http: &Client, bearer: &str) -> Result<Value, AgentError> {
        let resp = http
            .get(format!("{}{SESSION_PATH}", self.url))
            .timeout(Duration::from_secs(10))
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|e| AgentError::Network(short(&e.to_string())))?;
        let body = read_bounded(resp, 16 * 1024).await?;
        serde_json::from_slice(&body)
            .map_err(|e| AgentError::Protocol(format!("session: {}", short(&e.to_string()))))
    }
}

/// Read a response body, refusing one larger than `max` bytes. The body is
/// streamed so an oversized answer is abandoned, not buffered.
pub async fn read_bounded(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, AgentError> {
    use futures::StreamExt;
    if let Some(len) = resp.content_length() {
        if len as usize > max {
            return Err(AgentError::Protocol("response too large".into()));
        }
    }
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AgentError::Network(short(&e.to_string())))?;
        if out.len() + chunk.len() > max {
            return Err(AgentError::Protocol("response too large".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The cap every HTTP read in this adapter uses.
pub const MAX_HTTP_BODY: usize = MAX_PAYLOAD_BYTES;

/// Percent-encode a query value. Tickets are base64url and never need it,
/// but the URL must stay well-formed whatever the server hands back.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Errors shown to callers are one short line; the rest goes to the log.
pub fn short(message: &str) -> String {
    let line = message.lines().next().unwrap_or("");
    let mut s: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        s.push_str("...");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_ws_url_and_normalizes_the_base() {
        let ep = T3Endpoint::new("127.0.0.1:37731/", T3Credential::None);
        assert_eq!(ep.url, "http://127.0.0.1:37731");
        assert_eq!(ep.ws_url, "ws://127.0.0.1:37731/ws");
        let ep = T3Endpoint::new(
            "https://t3.example.ts.net",
            T3Credential::Bearer("x".into()),
        );
        assert_eq!(ep.ws_url, "wss://t3.example.ts.net/ws");
        assert_eq!(ep.credential_kind(), "bearer");
        assert_eq!(
            ep.ws_url_with_ticket("abc.def/+="),
            "wss://t3.example.ts.net/ws?wsTicket=abc.def%2F%2B%3D"
        );
    }

    #[test]
    fn debug_never_prints_the_credential() {
        let ep = T3Endpoint::new(
            "http://h",
            T3Credential::Pairing("super-secret-token".into()),
        );
        let s = format!("{ep:?}");
        assert!(!s.contains("super-secret"));
        assert!(s.contains("pairing"));
    }

    #[test]
    fn parses_the_captured_descriptor() {
        let raw = include_str!("fixtures/environment.json");
        let d: EnvironmentDescriptor = serde_json::from_str(raw).expect("descriptor");
        assert_eq!(d.server_version, "0.0.42");
        assert_eq!(d.protocol_version(), 1, "absent means protocol 1");
        assert!(d.capability("connectionProbe"));
        assert!(
            d.capability("fileAttachments"),
            "an object-valued flag counts as present"
        );
        assert!(!d.capability("agentActivityPublishing"));
        assert!(!d.capability("no-such-flag"));
    }

    #[test]
    fn short_is_one_bounded_line() {
        let long = format!("{}\nsecond line", "x".repeat(400));
        let s = short(&long);
        assert!(s.starts_with("xxx"));
        assert!(s.ends_with("..."));
        assert!(!s.contains("second"));
    }
}
