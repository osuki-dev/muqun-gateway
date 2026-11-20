//! Checks Desktop's own release channel. Installation uses the website's
//! platform-specific packages so a signed app is replaced as a whole.
use super::installer::{self, Phase};
use anyhow::{ensure, Context as _};
use muqun_gateway::ReleaseVersion;
use std::{
    path::PathBuf,
    process::Command,
    sync::{atomic::AtomicBool, OnceLock},
    time::Duration,
};

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
    Manual(String),
    Installing {
        version: String,
        phase: Option<Phase>,
    },
    InstallFailed(String),
    Ready {
        version: String,
        executable: PathBuf,
    },
    NoRelease,
    Failed,
}

pub fn is_preview_build() -> bool {
    INSTALLED.contains("-preview.")
}

pub async fn check() -> Status {
    match check_endpoint(RELEASE_API, INSTALLED).await {
        Ok(Status::Available(version)) if Package::current().is_err() => Status::Manual(version),
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

/// The current executable is resolved before activation: after Linux replaces
/// its inode, /proc/self/exe points at a deleted file.
struct Package {
    installer: PathBuf,
    destination: PathBuf,
    executable: PathBuf,
}

impl Package {
    fn current() -> anyhow::Result<Self> {
        Self::at(std::env::current_exe()?, INSTALLED)
    }

    fn at(executable: PathBuf, version: &str) -> anyhow::Result<Self> {
        let path = &executable;
        ensure!(
            path.is_absolute() && path.is_file(),
            "Desktop executable unavailable"
        );
        #[cfg(target_os = "macos")]
        let (destination, helper) = {
            let app = path
                .ancestors()
                .nth(3)
                .context("missing application bundle")?;
            ensure!(
                app.ends_with("Muqun Desktop.app")
                    && path.ends_with("Contents/MacOS/muqun-desktop"),
                "not an installed Desktop bundle"
            );
            (
                app.parent()
                    .context("missing applications directory")?
                    .to_owned(),
                app.join("Contents/Resources/installers"),
            )
        };
        #[cfg(not(target_os = "macos"))]
        let (destination, helper) = {
            ensure!(
                path.ends_with("bin/muqun-desktop"),
                "not an installed Desktop package"
            );
            let prefix = path
                .ancestors()
                .nth(2)
                .context("missing installation prefix")?;
            (
                prefix.to_owned(),
                prefix.join("share/muqun-desktop/installers"),
            )
        };
        ensure!(
            std::fs::read_to_string(helper.join("VERSION"))?.trim() == version,
            "installed Desktop version changed"
        );
        let installer = helper.join("install-desktop.sh");
        ensure!(installer.is_file(), "missing packaged installer");
        Ok(Self {
            destination,
            installer,
            executable,
        })
    }

    fn install(
        &self,
        installed: &str,
        version: &str,
        progress: impl Fn(Phase),
    ) -> anyhow::Result<()> {
        let current = ReleaseVersion::parse(installed)?;
        let next = ReleaseVersion::parse(version)?;
        ensure!(
            current.is_preview() == next.is_preview()
                && next > current
                && !version.ends_with("-preview.0"),
            "invalid update target"
        );
        let mut command = Command::new("/bin/sh");
        command
            .arg(&self.installer)
            .arg("--tag")
            .arg(format!("desktop-v{version}"))
            .env("MUQUN_DESKTOP_REPLACE_VERSION", installed)
            .env(
                if cfg!(target_os = "macos") {
                    "MUQUN_DESKTOP_APPLICATIONS_DIR"
                } else {
                    "PREFIX"
                },
                &self.destination,
            );
        // The script bounds all network operations. Once the activation
        // transaction starts, let its rollback/commit finish without a kill.
        installer::run(command, &AtomicBool::new(false), None, progress)?;
        Self::at(self.executable.clone(), version)?;
        Ok(())
    }
}

pub fn install(version: String, progress: impl Fn(Phase)) -> Status {
    let operation = || -> anyhow::Result<PathBuf> {
        let package = Package::current()?;
        package.install(INSTALLED, &version, progress)?;
        Ok(package.executable)
    };
    match operation() {
        Ok(executable) => Status::Ready {
            version,
            executable,
        },
        Err(error) => {
            tracing::warn!(%error, "Desktop package update failed");
            Status::InstallFailed(version)
        }
    }
}

static RESTART: OnceLock<PathBuf> = OnceLock::new();

pub fn request_restart(path: PathBuf, version: &str) -> anyhow::Result<()> {
    let package = Package::at(path, version)?;
    RESTART
        .set(package.executable)
        .map_err(|_| anyhow::anyhow!("restart already requested"))
}

/// Called only on successful, normal shutdown after instance release and
/// config flush. The replacement never forwards its launch to the old process.
pub fn relaunch_if_requested() -> anyhow::Result<()> {
    if let Some(executable) = RESTART.get() {
        use std::os::unix::process::CommandExt;
        return Err(Command::new(executable).exec().into());
    }
    Ok(())
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

    #[cfg(target_os = "linux")]
    #[test]
    fn updating_a_custom_installation_keeps_its_destination_and_refuses_a_downgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("user tools");
        let helpers = prefix.join("share/muqun-desktop/installers");
        std::fs::create_dir_all(&helpers).unwrap();
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        let executable = prefix.join("bin/muqun-desktop");
        std::fs::write(&executable, b"old fixture").unwrap();
        std::fs::write(helpers.join("VERSION"), "1.0.0").unwrap();
        std::fs::write(
            helpers.join("install-desktop.sh"),
            r#"set -eu
[ "$#" -eq 2 ] && [ "$1" = --tag ] && [ "$2" = desktop-v1.1.0 ]
[ "$MUQUN_DESKTOP_REPLACE_VERSION" = 1.0.0 ]
[ "$MUQUN_INSTALL_PROGRESS" = 1 ]
printf 'MUQUN_PROGRESS download 23 23\nMUQUN_PROGRESS verify\nMUQUN_PROGRESS install\n' >&2
echo 1.1.0 > "$PREFIX/share/muqun-desktop/installers/VERSION"
"#,
        )
        .unwrap();
        let package = Package::at(executable.clone(), "1.0.0").unwrap();
        assert!(package.install("1.0.0", "0.9.9", |_| {}).is_err());
        assert!(package.install("1.0.0", "1.1.0-preview.1", |_| {}).is_err());
        let observed = std::cell::RefCell::new(Vec::new());
        package
            .install("1.0.0", "1.1.0", |phase| observed.borrow_mut().push(phase))
            .unwrap();
        assert_eq!(
            observed.into_inner(),
            [
                Phase::Downloading {
                    received: 23,
                    total: 23
                },
                Phase::Verifying,
                Phase::Installing
            ]
        );
        assert!(Package::at(executable.clone(), "1.0.0").is_err());
        assert!(Package::at(executable, "1.1.0").is_ok());
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
