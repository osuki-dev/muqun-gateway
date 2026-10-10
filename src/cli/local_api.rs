//! The CLI's way to ask the running gateway something: raw HTTP/1.1 over
//! loopback with the admin token from `pairing.json`.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::Context as _;
use serde_json::Value;

use crate::{local_management_addr, read_pairing_file, Config};

/// Nothing is listening on the gateway's management address.
#[derive(Debug)]
pub(crate) struct GatewayNotRunning;

impl std::fmt::Display for GatewayNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the gateway is not running")
    }
}

impl std::error::Error for GatewayNotRunning {}

// Bounds what a stray listener on the port can make the CLI buffer. The
// gateway's own discovery answer (`/api/capabilities`, with every agent's model
// catalogue) is already past 64 KiB, which silently read as "no planes".
const MAX_RESPONSE: u64 = 4 * 1024 * 1024;

fn build_request(
    method: &str,
    path: &str,
    address: SocketAddr,
    token: &str,
    body: Option<&Value>,
) -> String {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n"
    );
    match body {
        Some(body) => {
            let body = body.to_string();
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ));
        }
        None => request.push_str("\r\n"),
    }
    request
}

/// Status code and JSON body (`Null` when the body is empty or not JSON).
fn parse_response(response: &str) -> anyhow::Result<(u16, Value)> {
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("invalid response from the gateway")?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .context("invalid response from the gateway")?;
    Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

/// The `serverId` whatever answers at `address` gives for itself on the
/// public `GET /api/capabilities` -- asked with no credential at all, so a
/// stranger on that port learns nothing. `None`: it answered without one.
pub(crate) fn identity_at(address: SocketAddr) -> anyhow::Result<Option<String>> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request =
        format!("GET /api/capabilities HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    (&mut stream)
        .take(MAX_RESPONSE)
        .read_to_string(&mut response)?;
    let (_, value) = parse_response(&response)?;
    Ok(value["serverId"].as_str().map(str::to_owned))
}

/// Send one authenticated request to the gateway on this machine.
pub(crate) fn admin_request(
    config: &Config,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> anyhow::Result<(u16, Value)> {
    admin_request_with_timeouts(
        config,
        method,
        path,
        body,
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
}

pub(crate) fn admin_request_with_timeouts(
    config: &Config,
    method: &str,
    path: &str,
    body: Option<&Value>,
    connect: Duration,
    io: Duration,
) -> anyhow::Result<(u16, Value)> {
    let pairing = read_pairing_file()?;
    let listen: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    let address = local_management_addr(listen);
    let mut stream = match TcpStream::connect_timeout(&address, connect) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(GatewayNotRunning.into());
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(io))?;
    stream.set_write_timeout(Some(io))?;
    let request = build_request(method, path, address, &pairing.payload.token, body);
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    (&mut stream)
        .take(MAX_RESPONSE)
        .read_to_string(&mut response)?;
    parse_response(&response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_carries_line_admin_token_and_length() {
        let body = serde_json::json!({ "ancestor_pids": [1, 2] });
        let request = build_request(
            "POST",
            "/api/mcp/tokens",
            "127.0.0.1:23999".parse().unwrap(),
            "pair-token",
            Some(&body),
        );
        assert!(request.starts_with("POST /api/mcp/tokens HTTP/1.1\r\n"));
        assert!(request.contains("Authorization: Bearer pair-token\r\n"));
        let text = body.to_string();
        assert!(request.contains(&format!("Content-Length: {}\r\n", text.len())));
        assert!(request.ends_with(&format!("\r\n\r\n{text}")));
        let bare = build_request("GET", "/x", "127.0.0.1:1".parse().unwrap(), "t", None);
        assert!(bare.ends_with("Connection: close\r\n\r\n"));
    }

    #[test]
    fn response_yields_status_and_json() {
        let (status, body) =
            parse_response("HTTP/1.1 404 Not Found\r\nX: y\r\n\r\n{\"error\":\"pane_not_found\"}")
                .unwrap();
        assert_eq!(
            (status, body["error"].as_str()),
            (404, Some("pane_not_found"))
        );
        assert_eq!(
            parse_response("HTTP/1.1 200 OK\r\n\r\n").unwrap().1,
            Value::Null
        );
        assert!(parse_response("garbage").is_err());
    }
}
