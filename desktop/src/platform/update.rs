//! Checks Desktop's own release channel. Installation uses the website's
//! platform-specific packages so a signed app is replaced as a whole.
use anyhow::{ensure, Context as _};
use muqun_gateway::ReleaseVersion;
use std::time::Duration;

pub const INSTALL_URL: &str = "https://muqun.dev/desktop/#install";
const RELEASE_API: &str = "https://muqun.dev/api/releases/desktop";
const INSTALLED: &str = env!("CARGO_PKG_VERSION");

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    Current,
    Available(String),
    NoRelease,
    Failed,
}

pub fn is_preview_build() -> bool {
    INSTALLED.contains("-preview.")
}

pub async fn check() -> Status {
    match check_endpoint(RELEASE_API, INSTALLED).await {
        Ok(status) => status,
        Err(error) => {
            tracing::debug!(%error, "Desktop release check failed");
            Status::Failed
        }
    }
}

async fn check_endpoint(base: &str, installed: &str) -> anyhow::Result<Status> {
    let current = ReleaseVersion::parse(installed)?;
    let channel = if current.is_preview() {
        "preview"
    } else {
        "stable"
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut response = client.get(format!("{base}/{channel}")).send().await?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "release check returned HTTP {}",
        response.status()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 8192,
            "oversized release response"
        );
        bytes.extend_from_slice(&chunk);
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        body["channel"].as_str() == Some(channel),
        "unexpected release channel"
    );
    let version = body.get("version").context("missing release version")?;
    if version.is_null() {
        return Ok(Status::NoRelease);
    }
    let value = version.as_str().context("invalid release version")?;
    let latest = ReleaseVersion::parse(value)?;
    ensure!(
        latest.is_preview() == current.is_preview() && !value.ends_with("-preview.0"),
        "invalid release for this channel"
    );
    Ok(if latest > current {
        Status::Available(value.to_owned())
    } else {
        Status::Current
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// Exercise the same HTTP request a packaged Desktop makes, with a local
    /// server returning the catalog contract and no dependency on a live release.
    async fn fixture(
        installed: &str,
        channel: &str,
        status: u16,
        json: &str,
    ) -> anyhow::Result<Status> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/api/releases/desktop", listener.local_addr()?);
        let expected = format!("GET /api/releases/desktop/{channel} HTTP/1.1\r\n");
        let response = format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}", json.len());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut input = tokio::io::BufReader::new(&mut stream);
            let mut line = String::new();
            input.read_line(&mut line).await.unwrap();
            assert_eq!(line, expected);
            loop {
                line.clear();
                if input.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = check_endpoint(&url, installed).await;
        server.await?;
        result
    }

    #[tokio::test]
    async fn release_check_selects_the_installed_channel_and_compares_versions_numerically() {
        let released = r#"{"channel":"preview","version":"1.1.0-preview.12"}"#;
        assert_eq!(
            fixture("1.1.0-preview.9", "preview", 200, released)
                .await
                .unwrap(),
            Status::Available("1.1.0-preview.12".into())
        );
        assert_eq!(
            fixture("1.1.0-preview.13", "preview", 200, released)
                .await
                .unwrap(),
            Status::Current
        );
        assert_eq!(
            fixture(
                "1.10.0",
                "stable",
                200,
                r#"{"channel":"stable","version":"1.10.0"}"#
            )
            .await
            .unwrap(),
            Status::Current
        );
        assert!(fixture("1.0.0", "stable", 200, released).await.is_err());
        assert!(fixture(
            "1.0.0",
            "stable",
            200,
            r#"{"channel":"stable","version":"1.1.0-preview.12"}"#
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn release_check_distinguishes_unpublished_from_failed_lookup() {
        let none = r#"{"channel":"stable","version":null}"#;
        assert_eq!(
            fixture("1.0.0", "stable", 200, none).await.unwrap(),
            Status::NoRelease
        );
        assert!(fixture("1.0.0", "stable", 502, none).await.is_err());
        assert!(fixture("1.0.0", "stable", 200, r#"{"channel":"stable"}"#)
            .await
            .is_err());
    }
}
