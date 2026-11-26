//! Checks Desktop's own release channel. Installation uses the website's
//! platform-specific packages so a signed app is replaced as a whole.
use super::installer::{self, Phase};
use anyhow::{ensure, Context as _};
use muqun_gateway::ReleaseVersion;
use std::{
    io::Read as _,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, OnceLock},
    time::{Duration, Instant},
};

pub const INSTALL_URL: &str = "https://muqun.dev/desktop/#install";
const RELEASE_API: &str = "https://muqun.dev/api/releases/desktop";
pub const GATEWAY_INSTALL_URL: &str = "https://muqun.dev/#install";
const GATEWAY_RELEASE_API: &str = "https://muqun.dev/api/releases/gateway";
const GATEWAY_INSTALL_TIMEOUT: Duration = Duration::from_secs(180);
const GATEWAY_OUTPUT_LIMIT: usize = 32 * 1024;
const INSTALLED: &str = env!("CARGO_PKG_VERSION");

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub enum GatewayStatus {
    #[default]
    Idle,
    Checking,
    Standalone,
    Current {
        installed: String,
    },
    Available {
        installed: String,
        latest: String,
        channel: String,
        can_install: bool,
    },
    Manual {
        installed: String,
        latest: String,
        channel: String,
        remote: bool,
    },
    NoRelease {
        installed: String,
    },
    Installing {
        version: String,
    },
    Updated {
        version: String,
    },
    InstallFailed {
        version: String,
        channel: String,
        error: String,
    },
    Failed(String),
}

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

pub fn installed_package() -> bool {
    Package::current().is_ok()
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

async fn fetch_release_version(base: &str, channel: &str) -> anyhow::Result<Option<String>> {
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
        return Ok(None);
    }
    let value = version.as_str().context("invalid release version")?;
    Ok(Some(value.to_owned()))
}

async fn check_endpoint(base: &str, installed: &str) -> anyhow::Result<Status> {
    let current = ReleaseVersion::parse(installed)?;
    let channel = if current.is_preview() {
        "preview"
    } else {
        "stable"
    };
    let Some(value) = fetch_release_version(base, channel).await? else {
        return Ok(Status::NoRelease);
    };
    let latest = ReleaseVersion::parse(&value)?;
    ensure!(
        latest.is_preview() == current.is_preview() && !value.ends_with("-preview.0"),
        "invalid release for this channel"
    );
    Ok(if latest > current {
        Status::Available(value)
    } else {
        Status::Current
    })
}

pub async fn check_gateway(installed: Option<String>, is_local: bool) -> GatewayStatus {
    check_gateway_endpoint(
        GATEWAY_RELEASE_API,
        installed,
        is_local,
        is_preview_build(),
        gateway_installer_script().is_some(),
    )
    .await
}

async fn check_gateway_endpoint(
    base: &str,
    installed: Option<String>,
    is_local: bool,
    prefer_preview: bool,
    has_installer: bool,
) -> GatewayStatus {
    let Some(raw_installed) = installed else {
        return GatewayStatus::Standalone;
    };
    let installed = raw_installed.trim().trim_start_matches('v').to_owned();
    let Ok(current) = ReleaseVersion::parse(&installed) else {
        return GatewayStatus::Failed("Unrecognized gateway version".into());
    };
    let channel = if current.is_preview() || prefer_preview {
        "preview"
    } else {
        "stable"
    };
    let value = match fetch_release_version(base, channel).await {
        Ok(Some(value)) => value,
        Ok(None) => return GatewayStatus::NoRelease { installed },
        Err(error) => {
            tracing::debug!(%error, "Gateway release check failed");
            return GatewayStatus::Failed(error.to_string());
        }
    };
    let cleaned = value.trim().trim_start_matches('v');
    let Ok(latest) = ReleaseVersion::parse(cleaned) else {
        return GatewayStatus::Failed("Invalid release version".into());
    };
    let expect_preview = channel == "preview";
    if latest.is_preview() != expect_preview || cleaned.ends_with("-preview.0") {
        return GatewayStatus::Failed("Invalid release for this channel".into());
    }
    if latest > current {
        let can_install = is_local && has_installer;
        if can_install {
            GatewayStatus::Available {
                installed,
                latest: cleaned.to_owned(),
                channel: channel.to_owned(),
                can_install: true,
            }
        } else {
            GatewayStatus::Manual {
                installed,
                latest: cleaned.to_owned(),
                channel: channel.to_owned(),
                remote: !is_local,
            }
        }
    } else {
        GatewayStatus::Current { installed }
    }
}

fn gateway_installer_script_at(executable: &Path) -> Option<PathBuf> {
    if !executable.is_absolute() {
        return None;
    }
    let prefix = executable.parent()?.parent()?;
    let candidate = if executable.ends_with("Contents/MacOS/muqun-desktop") {
        prefix.join("Resources/installers/install.sh")
    } else if executable.ends_with("bin/muqun-desktop") {
        prefix.join("share/muqun-desktop/installers/install.sh")
    } else {
        return None;
    };
    candidate.is_file().then_some(candidate)
}

fn gateway_installer_script() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(script) = gateway_installer_script_at(&exe) {
            return Some(script);
        }
    }
    #[cfg(debug_assertions)]
    {
        if let Some(script) = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join("install.sh"))
            .filter(|p| p.is_file())
        {
            return Some(script);
        }
    }
    None
}

fn gateway_installer_path_env() -> Option<std::ffi::OsString> {
    let mut entries = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if !path.as_os_str().is_empty() && !entries.contains(&path) {
            entries.push(path);
        }
    };
    if let Some(existing) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&existing) {
            push_unique(path);
        }
    }
    push_unique(PathBuf::from("/usr/local/bin"));
    push_unique(PathBuf::from("/opt/homebrew/bin"));
    if let Some(home) = dirs::home_dir() {
        push_unique(home.join(".local/bin"));
    }
    for sys in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        push_unique(PathBuf::from(sys));
    }
    std::env::join_paths(entries).ok()
}

fn validate_gateway_install_args(version: &str, channel: &str) -> Result<(), String> {
    if !matches!(channel, "stable" | "preview") {
        return Err("Invalid Gateway release channel".into());
    }
    let parsed =
        ReleaseVersion::parse(version).map_err(|_| "Invalid Gateway release version".to_owned())?;
    if version.ends_with("-preview.0") || parsed.is_preview() != (channel == "preview") {
        return Err("Gateway version does not match release channel".into());
    }
    Ok(())
}

fn last_error_line(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(240).collect())
}

fn install_gateway_with_script(
    script: &Path,
    version: String,
    channel: String,
    timeout: Duration,
) -> GatewayStatus {
    if let Err(error) = validate_gateway_install_args(&version, &channel) {
        return GatewayStatus::InstallFailed {
            version,
            channel,
            error,
        };
    }
    let (mut reader, writer) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(error) => {
            return GatewayStatus::InstallFailed {
                version,
                channel,
                error: error.to_string(),
            };
        }
    };
    if let Err(error) = reader.set_nonblocking(true) {
        return GatewayStatus::InstallFailed {
            version,
            channel,
            error: error.to_string(),
        };
    }
    let stderr_writer = match writer.try_clone() {
        Ok(cloned) => cloned,
        Err(error) => {
            return GatewayStatus::InstallFailed {
                version,
                channel,
                error: error.to_string(),
            };
        }
    };
    let mut command = Command::new("/bin/sh");
    command
        .arg(script)
        .arg("--channel")
        .arg(&channel)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::from(OwnedFd::from(stderr_writer)));
    if let Some(path) = gateway_installer_path_env() {
        command.env("PATH", path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is the only post-fork operation; no allocation or locking.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return GatewayStatus::InstallFailed {
                version,
                channel,
                error: error.to_string(),
            };
        }
    };
    drop(command);
    let started = Instant::now();
    let mut captured = Vec::new();
    let mut chunk = [0u8; 4096];
    let drain = |reader: &mut UnixStream, captured: &mut Vec<u8>, chunk: &mut [u8; 4096]| {
        while let Ok(count @ 1..) = reader.read(chunk) {
            captured.extend_from_slice(&chunk[..count]);
            if captured.len() > GATEWAY_OUTPUT_LIMIT {
                let excess = captured.len() - GATEWAY_OUTPUT_LIMIT;
                captured.drain(..excess);
            }
        }
    };
    loop {
        let exit = child.try_wait();
        drain(&mut reader, &mut captured, &mut chunk);
        match exit {
            Ok(Some(status)) => {
                if status.success() {
                    return GatewayStatus::Updated { version };
                }
                let error = last_error_line(&captured)
                    .unwrap_or_else(|| format!("Installer exited with {status}"));
                tracing::warn!(%error, %channel, %version, "Gateway installer failed");
                return GatewayStatus::InstallFailed {
                    version,
                    channel,
                    error,
                };
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return GatewayStatus::InstallFailed {
                        version,
                        channel,
                        error: "Gateway update timed out".into(),
                    };
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return GatewayStatus::InstallFailed {
                    version,
                    channel,
                    error: error.to_string(),
                };
            }
        }
    }
}

pub fn install_gateway(version: String, channel: String) -> GatewayStatus {
    if let Err(error) = validate_gateway_install_args(&version, &channel) {
        return GatewayStatus::InstallFailed {
            version,
            channel,
            error,
        };
    }
    let Some(script) = gateway_installer_script() else {
        return GatewayStatus::InstallFailed {
            version,
            channel,
            error: "Packaged Gateway installer is unavailable".into(),
        };
    };
    install_gateway_with_script(&script, version, channel, GATEWAY_INSTALL_TIMEOUT)
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
        use std::os::unix::fs::PermissionsExt;
        let path = &executable;
        let metadata = path.metadata()?;
        ensure!(
            path.is_absolute() && metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
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
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&executable, b"old fixture").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
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

    async fn gateway_fixture(
        installed: Option<&str>,
        expected_channel: &str,
        status: u16,
        json: &str,
        is_local: bool,
        prefer_preview: bool,
        has_installer: bool,
    ) -> GatewayStatus {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/releases/gateway",
            listener.local_addr().unwrap()
        );
        let expected = format!("GET /api/releases/gateway/{expected_channel} HTTP/1.1\r\n");
        let response = format!(
            "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
            json.len()
        );
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
        let result = check_gateway_endpoint(
            &url,
            installed.map(str::to_owned),
            is_local,
            prefer_preview,
            has_installer,
        )
        .await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn gateway_release_check_compares_versions_and_respects_local_vs_remote() {
        assert_eq!(
            check_gateway_endpoint("http://127.0.0.1:1", None, true, false, true).await,
            GatewayStatus::Standalone
        );
        assert_eq!(
            check_gateway_endpoint(
                "http://127.0.0.1:1",
                Some("not-a-version".into()),
                true,
                false,
                true
            )
            .await,
            GatewayStatus::Failed("Unrecognized gateway version".into())
        );

        let preview_release = r#"{"channel":"preview","version":"0.14.1-preview.35"}"#;
        // Local with packaged installer -> Available { can_install: true }
        assert_eq!(
            gateway_fixture(
                Some("v0.14.1-preview.34"),
                "preview",
                200,
                preview_release,
                true,
                false,
                true,
            )
            .await,
            GatewayStatus::Available {
                installed: "0.14.1-preview.34".into(),
                latest: "0.14.1-preview.35".into(),
                channel: "preview".into(),
                can_install: true,
            }
        );
        // Remote host -> Manual { remote: true } even when local installer exists
        assert_eq!(
            gateway_fixture(
                Some("0.14.1-preview.34"),
                "preview",
                200,
                preview_release,
                false,
                false,
                true,
            )
            .await,
            GatewayStatus::Manual {
                installed: "0.14.1-preview.34".into(),
                latest: "0.14.1-preview.35".into(),
                channel: "preview".into(),
                remote: true,
            }
        );
        // Local without packaged installer -> Manual { remote: false }
        assert_eq!(
            gateway_fixture(
                Some("0.14.1-preview.34"),
                "preview",
                200,
                preview_release,
                true,
                false,
                false,
            )
            .await,
            GatewayStatus::Manual {
                installed: "0.14.1-preview.34".into(),
                latest: "0.14.1-preview.35".into(),
                channel: "preview".into(),
                remote: false,
            }
        );
        // Already up to date
        assert_eq!(
            gateway_fixture(
                Some("0.14.1-preview.35"),
                "preview",
                200,
                preview_release,
                true,
                false,
                true,
            )
            .await,
            GatewayStatus::Current {
                installed: "0.14.1-preview.35".into(),
            }
        );
        // Unpublished channel -> NoRelease
        assert_eq!(
            gateway_fixture(
                Some("0.14.1"),
                "stable",
                200,
                r#"{"channel":"stable","version":null}"#,
                true,
                false,
                true,
            )
            .await,
            GatewayStatus::NoRelease {
                installed: "0.14.1".into(),
            }
        );
        // Mismatched channel in response -> Failed
        assert!(matches!(
            gateway_fixture(
                Some("0.14.1"),
                "stable",
                200,
                preview_release,
                true,
                false,
                true,
            )
            .await,
            GatewayStatus::Failed(_)
        ));

        // Verify packaged install.sh lookup for both Linux prefix and macOS bundle layouts
        let tmp = tempfile::tempdir().unwrap();
        let linux_script = tmp
            .path()
            .join("linux/share/muqun-desktop/installers/install.sh");
        std::fs::create_dir_all(linux_script.parent().unwrap()).unwrap();
        std::fs::write(&linux_script, "#!/bin/sh\n").unwrap();
        assert_eq!(
            gateway_installer_script_at(&tmp.path().join("linux/bin/muqun-desktop")),
            Some(linux_script)
        );
        let mac_script = tmp
            .path()
            .join("Muqun Desktop.app/Contents/Resources/installers/install.sh");
        std::fs::create_dir_all(mac_script.parent().unwrap()).unwrap();
        std::fs::write(&mac_script, "#!/bin/sh\n").unwrap();
        assert_eq!(
            gateway_installer_script_at(
                &tmp.path()
                    .join("Muqun Desktop.app/Contents/MacOS/muqun-desktop")
            ),
            Some(mac_script)
        );
        assert!(
            gateway_installer_script_at(&tmp.path().join("target/debug/muqun-desktop")).is_none()
        );
    }

    #[test]
    fn gateway_installer_rejects_invalid_channel_or_version() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("install.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
set -eu
[ "$#" -eq 2 ] && [ "$1" = "--channel" ]
if [ "$2" = "preview" ]; then
  echo "updated preview"
  exit 0
fi
echo "first detail line" >&2
echo "Error: simulated download failure" >&2
exit 1
"#,
        )
        .unwrap();

        for (version, channel) in [
            ("0.14.1", "nightly"),
            ("0.14.1", "stable; rm -rf /"),
            ("0.14.1", "--help"),
            ("not-a-version", "stable"),
            ("v0.14.1", "stable"),
            ("0.14.1;id", "stable"),
            ("0.14.1-preview.0", "preview"),
            ("0.14.1-preview.1", "stable"),
            ("0.14.1", "preview"),
        ] {
            let status = install_gateway_with_script(
                &script,
                version.into(),
                channel.into(),
                Duration::from_secs(5),
            );
            assert!(
                matches!(status, GatewayStatus::InstallFailed { .. }),
                "expected rejection for version={version:?} channel={channel:?}, got {status:?}"
            );
            let direct = install_gateway(version.into(), channel.into());
            assert!(matches!(direct, GatewayStatus::InstallFailed { .. }));
        }

        // Valid preview invocation succeeds
        assert_eq!(
            install_gateway_with_script(
                &script,
                "0.14.1-preview.35".into(),
                "preview".into(),
                Duration::from_secs(5),
            ),
            GatewayStatus::Updated {
                version: "0.14.1-preview.35".into(),
            }
        );

        // Failing script captures concise last error line
        assert_eq!(
            install_gateway_with_script(
                &script,
                "0.14.1".into(),
                "stable".into(),
                Duration::from_secs(5),
            ),
            GatewayStatus::InstallFailed {
                version: "0.14.1".into(),
                channel: "stable".into(),
                error: "Error: simulated download failure".into(),
            }
        );
    }
}
