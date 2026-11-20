//! Paired clients of this computer's Gateway, independent of the selected peer.
//! Discovery supplies the existing per-process loopback credential. Only device
//! display fields leave this module.
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum List {
    #[default]
    Idle,
    Loading,
    Ready(Vec<Client>),
    Unavailable,
    Failed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Client {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub last_seen_unix_ms: Option<u64>,
}

pub async fn load() -> List {
    let Some(local) = crate::hub::local_gateway_credential() else {
        return List::Unavailable;
    };
    fetch(&local).await.unwrap_or(List::Failed)
}

async fn fetch(local: &crate::hub::LocalGateway) -> anyhow::Result<List> {
    #[derive(Deserialize)]
    struct Response {
        devices: Vec<Client>,
    }
    let (client, root) = local_client(local)?;
    let response = client
        .get(root.join("/api/pairings")?)
        .bearer_auth(&local.token)
        .send()
        .await?
        .error_for_status()?;
    let mut devices = response.json::<Response>().await?.devices;
    devices.sort_by(|a, b| {
        b.last_seen_unix_ms
            .cmp(&a.last_seen_unix_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(List::Ready(devices))
}

pub(super) fn local_client(
    local: &crate::hub::LocalGateway,
) -> anyhow::Result<(reqwest::Client, reqwest::Url)> {
    let root = reqwest::Url::parse(&local.url)?;
    anyhow::ensure!(
        matches!(root.scheme(), "http" | "https")
            && root.username().is_empty()
            && root.password().is_none(),
        "invalid local Gateway URL"
    );
    let host = root.host_str().unwrap_or_default().trim_matches(['[', ']']);
    anyhow::ensure!(
        host.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || host == "localhost",
        "expected loopback"
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    Ok((client, root))
}

pub async fn revoke(id: &str) -> anyhow::Result<()> {
    let local = crate::hub::local_gateway_credential()
        .ok_or_else(|| anyhow::anyhow!("Local Gateway unavailable"))?;
    revoke_at(&local, id).await
}

async fn revoke_at(local: &crate::hub::LocalGateway, id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!matches!(id, "" | "." | ".."), "Invalid device ID");
    let (client, root) = local_client(local)?;
    let mut endpoint = root.join("/api/pairings")?;
    endpoint
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid Gateway URL"))?
        .push(id);
    let response = client
        .delete(endpoint)
        .bearer_auth(&local.token)
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    if status == reqwest::StatusCode::NOT_FOUND && body["error"]["code"] == "device_not_found" {
        return Ok(());
    }
    anyhow::ensure!(status.is_success(), "Gateway returned HTTP {status}");
    anyhow::ensure!(
        body["ok"] == true && body["revoked"] == id,
        "Gateway did not confirm device revocation"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::LocalGateway;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn local_pairings_are_read_with_the_local_credential_and_sorted_by_recent_use() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = LocalGateway {
            url: format!("http://{}", listener.local_addr().unwrap()),
            token: "fixture-local-client".into(),
            device_id: None,
            transport_key: None,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).await.unwrap();
                if n == 0 || line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            assert!(headers.starts_with("GET /api/pairings HTTP/1.1\r\n"));
            assert!(headers
                .to_lowercase()
                .contains("authorization: bearer fixture-local-client\r\n"));
            let body = r#"{"devices":[{"id":"phone","name":"Phone","last_seen_unix_ms":20},{"id":"mini","name":"Mini","kind":"gateway","last_seen_unix_ms":40}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let List::Ready(clients) = fetch(&local).await.unwrap() else {
            panic!("missing paired clients")
        };
        assert_eq!(
            clients.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["Mini", "Phone"]
        );
        server.await.unwrap();
    }
}
