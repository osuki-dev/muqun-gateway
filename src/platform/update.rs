//! Explicit, stable-only self-update. HTTPS + SHA256 provide transport/integrity,
//! not publisher signing. No alternate production origin or network retries.
use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::lifecycle::{Action, Controller};

const REPO: &str = "osuki-dev/muqun-gateway";
const TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];
const MAX_BINARY: u64 = 256 * 1024 * 1024;
const BACKUP: &str = ".muqun-gateway.update-backup";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version([u64; 3]);

impl Version {
    fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('.').collect();
        anyhow::ensure!(parts.len() == 3, "updates require stable X.Y.Z versions; use your original installer for development/prerelease builds");
        let mut version = [0; 3];
        for (index, part) in parts.iter().enumerate() {
            anyhow::ensure!(
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0')),
                "invalid stable X.Y.Z version"
            );
            version[index] = part.parse().context("version component is too large")?;
        }
        Ok(Self(version))
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    state: String,
}

pub(crate) struct Plan {
    current: String,
    tag: String,
    binary: Option<Asset>,
    checksum: Option<Asset>,
}

impl Plan {
    pub(crate) fn available(&self) -> bool {
        self.binary.is_some()
    }

    pub(crate) fn summary(&self) -> String {
        if self.available() {
            format!("Gateway {} → {} available (official stable release; SHA256 integrity, not signing).", self.current, self.tag)
        } else {
            format!(
                "Gateway {}: no newer stable release (latest {}). No download or restart.",
                self.current, self.tag
            )
        }
    }
}

fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok(TARGETS[0]),
        ("macos", "x86_64") => Ok(TARGETS[1]),
        ("linux", "x86_64") => Ok(TARGETS[2]),
        ("linux", "aarch64") => Ok(TARGETS[3]),
        _ => anyhow::bail!("no supported release binary for this platform"),
    }
}

fn release_url(tag: &str, name: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{name}")
}

fn plan(release: Release, current: &str, target: &str) -> Result<Plan> {
    let installed = Version::parse(current)?;
    anyhow::ensure!(
        !release.draft && !release.prerelease,
        "latest release is not stable; no update attempted"
    );
    let latest = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .context("release tag must be vX.Y.Z")?,
    )?;
    let mut result = Plan {
        current: current.into(),
        tag: release.tag_name,
        binary: None,
        checksum: None,
    };
    if latest <= installed {
        return Ok(result);
    }
    anyhow::ensure!(TARGETS.contains(&target), "unsupported release target");
    // Matrix uploads can finish at different times. Require the entire release,
    // not just whichever asset happened to be present when this host checked.
    for platform in TARGETS {
        let name = format!("muqun-gateway-{platform}");
        for (name, max) in [(name.clone(), MAX_BINARY), (format!("{name}.sha256"), 1024)] {
            let assets: Vec<_> = release
                .assets
                .iter()
                .filter(|asset| asset.name == name)
                .collect();
            anyhow::ensure!(assets.len() == 1,
                "release {} is incomplete or has no SHA256 checksums; wait for a newer checksum-equipped complete release (no unsafe binary installed)", result.tag);
            let asset = assets[0];
            anyhow::ensure!(
                asset.state == "uploaded" && asset.size > 0 && asset.size <= max,
                "release asset is incomplete or exceeds its size limit; no update attempted"
            );
            anyhow::ensure!(
                asset.browser_download_url == release_url(&result.tag, &name),
                "release asset is not at the official version-locked URL; no update attempted"
            );
            if platform == target {
                if name.ends_with(".sha256") {
                    result.checksum = Some(asset.clone());
                } else {
                    result.binary = Some(asset.clone());
                }
            }
        }
    }
    Ok(result)
}

fn trusted_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "api.github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .user_agent(concat!("muqun-gateway/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || !trusted_url(attempt.url()) {
                attempt.error("update redirect refused")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| anyhow::anyhow!("cannot initialize secure update HTTP client"))
}

async fn response(client: &reqwest::Client, url: &str, max: u64) -> Result<reqwest::Response> {
    // Do not propagate reqwest's URL-bearing error: CDN queries are signed.
    let response = client.get(url).send().await.map_err(|_| {
        anyhow::anyhow!(
            "update request failed (network, TLS, redirect or timeout); no automatic retry"
        )
    })?;
    anyhow::ensure!(
        response.status().is_success(),
        "update server returned HTTP {}; no update attempted",
        response.status().as_u16()
    );
    anyhow::ensure!(
        response.content_length().is_none_or(|size| size <= max),
        "update response exceeds size limit"
    );
    Ok(response)
}

async fn bounded_body(mut response: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("update download failed or timed out"))?
    {
        anyhow::ensure!(
            body.len() as u64 + chunk.len() as u64 <= max,
            "update response exceeds size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) async fn check() -> Result<Plan> {
    // Checking needs no config or mutable state. The running manager may still
    // be the old image after a successful update; inspect the installed image.
    let exe = super::setup::executable_path()?;
    let current = if exe.is_file() {
        executable_version(&exe).await?
    } else {
        env!("CARGO_PKG_VERSION").into()
    };
    check_at(
        &client()?,
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        &current,
    )
    .await
}

async fn check_at(client: &reqwest::Client, url: &str, current: &str) -> Result<Plan> {
    let body = bounded_body(response(client, url, 1024 * 1024).await?, 1024 * 1024).await?;
    let release: Release = serde_json::from_slice(&body)
        .map_err(|_| anyhow::anyhow!("invalid release metadata; no update attempted"))?;
    plan(release, current, target()?)
}

/// Run async update I/O from the synchronous raw-mode manager without nesting a
/// runtime. No UI/process handoff: the same manager PID continues after replace.
pub(crate) fn in_thread<T: Send + 'static, F: std::future::Future<Output = Result<T>>>(
    work: impl FnOnce() -> F + Send + 'static,
) -> Result<T> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(work())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("update worker failed"))?
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    Ok(OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}

#[cfg(unix)]
fn validate_destination(exe: &Path, home: &Path, install_dir: &Path) -> Result<fs::Metadata> {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid has no pointer or memory preconditions.
    let uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        uid != 0,
        "self-update never runs as root; use your package manager or original installer"
    );
    anyhow::ensure!(
        exe.is_absolute()
            && fs::canonicalize(exe)? == exe
            && fs::canonicalize(install_dir)? == install_dir,
        "symlink/ambiguous executable path; use the original installer instead"
    );
    let home = fs::canonicalize(home)?;
    anyhow::ensure!(exe == install_dir.join("muqun-gateway") && install_dir.starts_with(&home)
        && !exe.strip_prefix(&home)?.components().any(|part| matches!(part.as_os_str().to_str(), Some("target" | "plugins" | "node_modules" | ".cargo" | "Cellar" | ".linuxbrew" | ".nix-profile"))),
        "not a standalone user installation; use the package/source/plugin installer (default supported path: ~/.local/bin/muqun-gateway)");
    let file = fs::symlink_metadata(exe)?;
    let dir = fs::metadata(install_dir)?;
    anyhow::ensure!(file.is_file() && file.uid() == uid && file.nlink() == 1 && file.mode() & 0o300 == 0o300
        && file.mode() & 0o022 == 0 && file.mode() & 0o6000 == 0
        && dir.uid() == uid && dir.mode() & 0o200 != 0 && dir.mode() & 0o022 == 0,
        "executable/directory must be owned, writable, ordinary user files without shared write permissions; use original installer");
    for ancestor in install_dir
        .ancestors()
        .take_while(|path| path.starts_with(&home))
    {
        let metadata = fs::metadata(ancestor)?;
        anyhow::ensure!(
            metadata.uid() == uid && metadata.mode() & 0o022 == 0,
            "installation path has an unowned/shared-writable ancestor; use original installer"
        );
    }
    Ok(file)
}

#[cfg(unix)]
fn update_lock(parent: &Path) -> Result<File> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(parent.join(".muqun-gateway.update-lock"))?;
    let metadata = file.metadata()?;
    // SAFETY: no pointer preconditions; the owned file keeps the lock alive.
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0,
        "unsafe update lock file; inspect the installation directory"
    );
    anyhow::ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another update is in progress, or filesystem locking is unavailable"
    );
    Ok(file)
}

fn checksum(body: &[u8], name: &str) -> Result<[u8; 32]> {
    let text = std::str::from_utf8(body).context("invalid SHA256 file")?;
    let (digest, file) = text
        .trim_end_matches('\n')
        .split_once("  ")
        .context("invalid SHA256 format")?;
    anyhow::ensure!(
        file == name
            && digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid SHA256 filename or digest"
    );
    let mut hash = [0; 32];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[i * 2..i * 2 + 2], 16)?;
    }
    Ok(hash)
}

async fn download(
    client: &reqwest::Client,
    asset: &Asset,
    hash: [u8; 32],
    file: &mut File,
) -> Result<()> {
    let mut response = response(client, &asset.browser_download_url, MAX_BINARY).await?;
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("binary download failed or timed out"))?
    {
        size += chunk.len() as u64;
        anyhow::ensure!(
            size <= asset.size && size <= MAX_BINARY,
            "binary download exceeds declared size"
        );
        digest.update(&chunk);
        file.write_all(&chunk)?;
    }
    anyhow::ensure!(size == asset.size, "binary download is truncated");
    anyhow::ensure!(
        digest.finalize().as_slice() == hash,
        "SHA256 mismatch; binary was not executed or installed"
    );
    file.sync_all()?;
    Ok(())
}

fn verify_platform(path: &Path) -> Result<()> {
    let mut header = [0_u8; 20];
    File::open(path)?
        .read_exact(&mut header)
        .context("release binary is too short")?;
    let valid = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", arch) => {
            header[..7] == *b"\x7fELF\x02\x01\x01"
                && u16::from_le_bytes([header[18], header[19]])
                    == if arch == "x86_64" { 62 } else { 183 }
        }
        ("macos", arch) => {
            header[..4] == [0xcf, 0xfa, 0xed, 0xfe]
                && u32::from_le_bytes(header[4..8].try_into()?)
                    == if arch == "x86_64" {
                        0x01000007
                    } else {
                        0x0100000c
                    }
        }
        _ => false,
    };
    anyhow::ensure!(
        valid,
        "release binary does not match this platform; not executed or installed"
    );
    Ok(())
}

async fn executable_version(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt as _;
    let mut command = tokio::process::Command::new(path);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // Another thread can fork while the staging writer is open. Its CLOEXEC
    // copy briefly keeps the inode write-open after our descriptor closes.
    // ETXTBSY proves exec never occurred; retry only this pre-exec condition.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => {
                return Err(error).context("cannot run side-effect-free binary --version")
            }
        }
    };
    let mut output = child
        .stdout
        .take()
        .context("missing version output")?
        .take(257);
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await?;
        anyhow::ensure!(
            bytes.len() <= 256 && child.wait().await?.success(),
            "binary version probe failed"
        );
        let text = std::str::from_utf8(&bytes).context("invalid binary version output")?;
        let version = text
            .trim()
            .strip_prefix("muqun-gateway ")
            .context("unexpected binary version output")?;
        Version::parse(version)?;
        Ok(version.to_string())
    })
    .await;
    match result {
        Ok(Ok(version)) => Ok(version),
        failure => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match failure {
                Ok(Err(error)) => Err(error),
                _ => anyhow::bail!("binary version probe timed out"),
            }
        }
    }
}

#[cfg(unix)]
pub(crate) async fn apply(plan: Plan) -> Result<String> {
    use std::os::unix::fs::MetadataExt as _;
    let Some(binary) = plan.binary else {
        return Ok(plan.summary());
    };
    let exe = super::setup::executable_path()?;
    let home = dirs::home_dir().context("cannot resolve HOME")?;
    let install_dir = std::env::var_os("MUQUN_GATEWAY_INSTALL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin"));
    let before = validate_destination(&exe, &home, &install_dir)?;
    let _update_lock = update_lock(&install_dir)?;
    let backup = install_dir.join(BACKUP);
    anyhow::ensure!(!backup.try_exists()? && fs::symlink_metadata(&backup).is_err(),
        "an interrupted update backup exists; inspect .muqun-gateway.update-backup and recover with your original installer before retrying");
    let current = executable_version(&exe).await?;
    anyhow::ensure!(
        current == plan.current
            && Version::parse(plan.tag.trim_start_matches('v'))? > Version::parse(&current)?,
        "installed version changed since check; check again (no downgrade attempted)"
    );
    let client = client()?;
    let checksum_asset = plan
        .checksum
        .context("release has no SHA256 checksum; wait for a newer checksum-equipped release")?;
    let temporary = prepare(
        &client,
        &binary,
        &checksum_asset,
        &install_dir,
        before.mode() & 0o777,
        plan.tag.trim_start_matches('v'),
    )
    .await?;
    // No await/cancellation point after acquiring lifecycle ownership. The old
    // image remains executable at `exe` until atomic replacement and is retained
    // as an exact hard-link backup through readiness/rollback.
    tokio::task::spawn_blocking(move || {
        let _update_lock = _update_lock;
        let controller = Controller::acquire(Action::Restart)?;
        anyhow::ensure!(controller.exe() == exe, "lifecycle executable changed during update");
        let now = validate_destination(&exe, &home, &install_dir)?;
        anyhow::ensure!(now.dev() == before.dev() && now.ino() == before.ino() && now.len() == before.len() && now.mtime() == before.mtime(),
            "installed binary changed during download; check again");
        ensure_running_executable(&exe)?;
        install(&exe, &temporary.0, &backup, controller.was_running, |action| controller.control(action))?;
        Ok(format!("Updated to {}. {} Pairing/config and terminal tasks unchanged. Reopen manage to load the new UI.", plan.tag,
            if controller.was_running { "Gateway restarted through its previous owner." } else { "Gateway remains stopped." }))
    }).await.context("update transaction worker failed")?
}

#[cfg(unix)]
async fn prepare(
    client: &reqwest::Client,
    binary: &Asset,
    checksum_asset: &Asset,
    parent: &Path,
    mode: u32,
    version: &str,
) -> Result<Temporary> {
    use std::os::unix::fs::PermissionsExt as _;
    let body = bounded_body(
        response(client, &checksum_asset.browser_download_url, 1024).await?,
        1024,
    )
    .await?;
    anyhow::ensure!(
        body.len() as u64 == checksum_asset.size,
        "checksum size differs from release metadata"
    );
    let hash = checksum(&body, &binary.name)?;
    let temporary =
        Temporary(parent.join(format!(".muqun-gateway.update-{}", uuid::Uuid::new_v4())));
    let mut file = private_file(&temporary.0)?;
    download(client, binary, hash, &mut file).await?;
    drop(file); // A writable descriptor prevents executing the image on Linux.
    verify_platform(&temporary.0)?;
    fs::set_permissions(&temporary.0, fs::Permissions::from_mode(mode))?;
    anyhow::ensure!(
        executable_version(&temporary.0).await? == version,
        "release binary reports the wrong version; not installed"
    );
    Ok(temporary)
}

#[cfg(unix)]
fn ensure_running_executable(exe: &Path) -> Result<()> {
    if let Some(pid) = super::lifecycle::running_pid()? {
        #[cfg(target_os = "linux")]
        let running = fs::read_link(format!("/proc/{pid}/exe"))?;
        #[cfg(target_os = "macos")]
        let running = {
            let mut buffer = [0_u8; 4096];
            // SAFETY: valid writable buffer with the advertised capacity.
            let size = unsafe {
                libc::proc_pidpath(pid as i32, buffer.as_mut_ptr().cast(), buffer.len() as u32)
            };
            anyhow::ensure!(size > 0, "cannot verify running gateway executable");
            let end = buffer
                .iter()
                .position(|byte| *byte == 0)
                .context("invalid process path")?;
            PathBuf::from(std::str::from_utf8(&buffer[..end])?)
        };
        anyhow::ensure!(running == exe, "running gateway uses another executable; stop it using its own installation before updating");
    }
    Ok(())
}

fn sync_directory(exe: &Path) -> Result<()> {
    File::open(exe.parent().context("executable has no parent")?)?.sync_all()?;
    Ok(())
}

fn install(
    exe: &Path,
    temporary: &Path,
    backup: &Path,
    running: bool,
    mut control: impl FnMut(Action) -> Result<()>,
) -> Result<()> {
    fs::hard_link(exe, backup)
        .context("cannot reserve exact old binary backup; no downtime attempted")?;
    if let Err(error) = sync_directory(exe).and_then(|()| {
        if running {
            control(Action::Stop)
        } else {
            Ok(())
        }
    }) {
        fs::remove_file(backup)?;
        return Err(error).context("update did not replace the executable");
    }
    let outcome = fs::rename(temporary, exe)
        .map_err(Into::into)
        .and_then(|()| sync_directory(exe))
        .and_then(|()| {
            if running {
                control(Action::Start)
            } else {
                Ok(())
            }
        });
    if let Err(error) = outcome {
        // Quiesce the failed new image/supervisor before restoring. Exactly one
        // recovery start; never keep retrying a release that failed readiness.
        if running {
            control(Action::Stop).context("update failed and recovery stop failed; backup retained; recover with original installer")?;
        }
        fs::rename(backup, exe).context(
            "update failed and binary restore failed; backup retained for manual recovery",
        )?;
        sync_directory(exe)?;
        if running {
            control(Action::Start).context("old binary restored but gateway recovery failed; start it manually and inspect logs")?;
        }
        return Err(error)
            .context("update failed; exact previous binary and running state restored");
    }
    fs::remove_file(backup).context("updated successfully but backup cleanup failed; inspect installation before updating again")?;
    sync_directory(exe)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axum::{extract::State, http::StatusCode, response::IntoResponse, Router};
    use std::collections::HashMap;
    use std::os::unix::fs::{symlink, PermissionsExt as _};
    use std::sync::{Arc, Mutex};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/update-fixtures")
                .join(uuid::Uuid::new_v4().to_string());
            fs::create_dir_all(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            Self(root)
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
        fn binary(&self) -> PathBuf {
            let source = self.write(
                "fixture.rs",
                br#"fn main() {
                if std::env::args().nth(1).as_deref() == Some("--version") {
                    println!("muqun-gateway 0.14.0");
                } else { std::process::exit(42); }
            }"#,
            );
            let binary = self.0.join("fixture-binary");
            let status = std::process::Command::new("rustc")
                .arg("--crate-name")
                .arg("update_fixture")
                .arg(source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap();
            assert!(status.success());
            binary
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn complete_release(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: TARGETS
                .into_iter()
                .flat_map(|target| {
                    let name = format!("muqun-gateway-{target}");
                    [name.clone(), format!("{name}.sha256")].map(|name| Asset {
                        browser_download_url: release_url(tag, &name),
                        name,
                        size: 100,
                        state: "uploaded".into(),
                    })
                })
                .collect(),
        }
    }

    #[test]
    fn stable_semver_and_matrix_fail_closed() {
        for bad in [
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.3-beta",
            "1.2.3+build",
            "v1.2.3",
            "1.2.-3",
            "1.2.18446744073709551616",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
        assert!(Version::parse("0.14.0").unwrap() > Version::parse("0.9.99").unwrap());
        for installed in ["0.14.0", "0.15.0"] {
            assert!(
                !plan(complete_release("v0.14.0"), installed, target().unwrap())
                    .unwrap()
                    .available()
            );
        }
        let good = plan(complete_release("v0.14.0"), "0.13.0", target().unwrap()).unwrap();
        assert!(good.available());
        for missing in 0..8 {
            let mut release = complete_release("v0.14.0");
            release.assets.remove(missing);
            assert!(plan(release, "0.13.0", target().unwrap())
                .unwrap_err_string()
                .contains("checksum-equipped"));
        }
        let mut release = complete_release("v0.14.0");
        release.assets.push(release.assets[0].clone());
        assert!(plan(release, "0.13.0", target().unwrap()).is_err());
        let mut release = complete_release("v0.14.0");
        release.assets[0].browser_download_url = "https://evil.test/payload".into();
        assert!(plan(release, "0.13.0", target().unwrap()).is_err());
        let mut release = complete_release("v0.14.0");
        release.assets[0].size = MAX_BINARY + 1;
        assert!(plan(release, "0.13.0", target().unwrap()).is_err());
        let mut release = complete_release("v0.14.0");
        release.prerelease = true;
        assert!(plan(release, "0.13.0", target().unwrap()).is_err());
        assert!(plan(
            complete_release("v0.14.0"),
            "0.13.0-beta",
            target().unwrap()
        )
        .is_err());
    }

    trait ErrorString {
        fn unwrap_err_string(self) -> String;
    }
    impl<T> ErrorString for Result<T> {
        fn unwrap_err_string(self) -> String {
            match self {
                Err(error) => format!("{error:#}"),
                Ok(_) => panic!("expected refusal"),
            }
        }
    }

    #[test]
    fn redirects_and_checksum_formats_are_strict() {
        for url in [
            "http://github.com/a",
            "https://evil.test/a",
            "https://github.com.evil.test/a",
            "https://github.com:444/a",
            "https://user:secret@github.com/a",
        ] {
            assert!(!trusted_url(&reqwest::Url::parse(url).unwrap()));
        }
        for url in [
            "https://github.com/a",
            "https://release-assets.githubusercontent.com/a?signature=secret",
        ] {
            assert!(trusted_url(&reqwest::Url::parse(url).unwrap()));
        }
        let hash = "0".repeat(64);
        assert_eq!(
            checksum(format!("{hash}  binary\n").as_bytes(), "binary").unwrap(),
            [0; 32]
        );
        for bad in [
            format!("{hash}  other\n"),
            format!("{hash} *binary\n"),
            format!("{hash}  binary\nextra"),
            format!("{}  binary", "F".repeat(64)),
        ] {
            assert!(checksum(bad.as_bytes(), "binary").is_err());
        }
    }

    #[test]
    fn installed_destination_refuses_symlink_source_permissions_and_shared_files() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let parent = home.join(".local/bin");
        fs::create_dir_all(&parent).unwrap();
        let exe = parent.join("muqun-gateway");
        fs::write(&exe, b"old").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_destination(&exe, home, &parent).is_ok());
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(validate_destination(&exe, home, &parent).is_err());
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(validate_destination(&exe, home, &parent).is_err());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::hard_link(&exe, parent.join("shared")).unwrap();
        assert!(validate_destination(&exe, home, &parent).is_err());
        fs::remove_file(parent.join("shared")).unwrap();
        symlink(&parent, home.join("link")).unwrap();
        assert!(
            validate_destination(&home.join("link/muqun-gateway"), home, &home.join("link"))
                .is_err()
        );
        let source = home.join("target/release");
        fs::create_dir_all(&source).unwrap();
        fs::copy(&exe, source.join("muqun-gateway")).unwrap();
        assert!(validate_destination(&source.join("muqun-gateway"), home, &source).is_err());
    }

    #[test]
    fn update_lock_serializes_same_executable_across_different_state_dirs() {
        let fixture = Fixture::new();
        let held = update_lock(&fixture.0).unwrap();
        assert!(update_lock(&fixture.0)
            .unwrap_err_string()
            .contains("another update"));
        drop(held);
        assert!(update_lock(&fixture.0).is_ok());
    }

    #[test]
    fn atomic_install_preserves_stopped_state_and_rolls_back_once_on_start_failure() {
        let fixture = Fixture::new();
        let exe = fixture.write("muqun-gateway", b"exact old image");
        let temporary = fixture.write("new", b"new image");
        let backup = fixture.0.join(BACKUP);
        install(&exe, &temporary, &backup, false, |_| {
            panic!("stopped update must not control processes")
        })
        .unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new image");
        assert!(!backup.exists());
        fs::write(&exe, b"exact old image").unwrap();
        let temporary = fixture.write("new", b"new image");
        let mut calls = Vec::new();
        let error = install(&exe, &temporary, &backup, true, |action| {
            calls.push(action);
            if calls.len() == 2 {
                anyhow::bail!("new startup failed");
            }
            Ok(())
        })
        .unwrap_err_string();
        assert!(error.contains("exact previous binary and running state restored"));
        assert_eq!(
            calls,
            [Action::Stop, Action::Start, Action::Stop, Action::Start]
        );
        assert_eq!(fs::read(&exe).unwrap(), b"exact old image");
        assert!(!backup.exists());
    }

    #[test]
    fn launchd_update_survives_the_eio_window_on_start_and_on_recovery_start() {
        let launchd = super::super::launchd::fake::Fake::new();
        let fixture = Fixture::new();
        let backup = fixture.0.join(BACKUP);

        // Stop, replace, then a start that hits launchd still releasing the
        // old job: retried, not reported as a failed release.
        launchd.load();
        launchd.set("bootstrap", "5\n5\n0\n");
        let exe = fixture.write("muqun-gateway", b"old");
        let temporary = fixture.write("new", b"new");
        install(&exe, &temporary, &backup, true, |action| {
            launchd.launchctl().control(action)
        })
        .unwrap();
        assert!(launchd.loaded());
        assert_eq!(fs::read(&exe).unwrap(), b"new");

        // The new image is refused outright; the recovery start of the old one
        // then hits the EIO window too and must still bring the agent back.
        launchd.set("bootstrap", "78\n5\n5\n0\n");
        let temporary = fixture.write("newer", b"newer");
        let error = install(&exe, &temporary, &backup, true, |action| {
            launchd.launchctl().control(action)
        })
        .unwrap_err_string();
        assert!(error.contains("running state restored"), "{error}");
        assert!(launchd.loaded(), "the gateway was left unloaded");
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        assert!(!backup.exists());
    }

    #[test]
    fn stale_backup_stop_failure_and_failed_recovery_do_not_overwrite_evidence() {
        let fixture = Fixture::new();
        let exe = fixture.write("muqun-gateway", b"old");
        let temporary = fixture.write("new", b"new");
        let backup = fixture.write(BACKUP, b"prior recovery evidence");
        assert!(install(&exe, &temporary, &backup, true, |_| panic!("no downtime")).is_err());
        assert_eq!(fs::read(&backup).unwrap(), b"prior recovery evidence");
        fs::remove_file(&backup).unwrap();
        assert!(install(&exe, &temporary, &backup, true, |_| anyhow::bail!(
            "stop refused"
        ))
        .is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert!(!backup.exists());
        let mut calls = 0;
        assert!(install(&exe, &temporary, &backup, true, |_| {
            calls += 1;
            if calls > 1 {
                anyhow::bail!("supervisor unavailable");
            }
            Ok(())
        })
        .unwrap_err_string()
        .contains("backup retained"));
        assert_eq!(calls, 3);
        assert_eq!(fs::read(&backup).unwrap(), b"old");
    }

    type Replies = Arc<Mutex<HashMap<String, (StatusCode, Vec<u8>)>>>;
    struct Server {
        url: String,
        replies: Replies,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Server {
        async fn new() -> Self {
            let replies: Replies = Arc::default();
            let requests: Arc<Mutex<Vec<String>>> = Arc::default();
            async fn serve(
                State((replies, requests)): State<(Replies, Arc<Mutex<Vec<String>>>)>,
                uri: axum::http::Uri,
            ) -> impl IntoResponse {
                requests.lock().unwrap().push(uri.path().to_string());
                if uri.path() == "/stall" {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                replies
                    .lock()
                    .unwrap()
                    .get(uri.path())
                    .cloned()
                    .unwrap_or((StatusCode::NOT_FOUND, Vec::new()))
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .fallback(serve)
                .with_state((replies.clone(), requests.clone()));
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                url,
                replies,
                requests,
                task,
            }
        }
        fn set(&self, path: &str, status: StatusCode, body: Vec<u8>) {
            self.replies
                .lock()
                .unwrap()
                .insert(path.into(), (status, body));
        }
        fn asset(&self, path: &str, body: Vec<u8>) -> Asset {
            let size = body.len() as u64;
            self.set(path, StatusCode::OK, body);
            Asset {
                name: path.trim_start_matches('/').into(),
                browser_download_url: format!("{}{path}", self.url),
                size,
                state: "uploaded".into(),
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn test_client() -> reqwest::Client {
        // Only direct test injection permits loopback HTTP; production has no
        // URL/env override and requires HTTPS and the official matrix URLs.
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn check_without_update_fetches_only_metadata_and_never_restarts() {
        let server = Server::new().await;
        server.set(
            "/latest",
            StatusCode::OK,
            br#"{"tag_name":"v0.13.0","draft":false,"prerelease":false,"assets":[]}"#.to_vec(),
        );
        let result = check_at(&test_client(), &format!("{}/latest", server.url), "0.13.0")
            .await
            .unwrap();
        assert!(!result.available());
        assert!(apply(result)
            .await
            .unwrap()
            .contains("No download or restart"));
        assert_eq!(*server.requests.lock().unwrap(), ["/latest"]);
    }

    #[tokio::test]
    async fn isolated_http_fixture_covers_status_size_truncation_hash_and_network_errors() {
        let fixture = Fixture::new();
        let server = Server::new().await;
        let client = test_client();
        let asset = server.asset("/binary", b"payload".to_vec());
        let mut file = private_file(&fixture.0.join("download")).unwrap();
        assert!(download(&client, &asset, [0; 32], &mut file)
            .await
            .unwrap_err_string()
            .contains("SHA256 mismatch"));
        let mut truncated = asset.clone();
        truncated.size += 1;
        assert!(download(&client, &truncated, [0; 32], &mut file)
            .await
            .unwrap_err_string()
            .contains("truncated"));
        let mut too_small = asset.clone();
        too_small.size -= 1;
        assert!(download(&client, &too_small, [0; 32], &mut file)
            .await
            .unwrap_err_string()
            .contains("declared size"));
        assert!(response(&client, &asset.browser_download_url, 1)
            .await
            .unwrap_err_string()
            .contains("size limit"));
        server.set(
            "/binary",
            StatusCode::SERVICE_UNAVAILABLE,
            b"secret backend body".to_vec(),
        );
        let error = download(&client, &asset, [0; 32], &mut file)
            .await
            .unwrap_err_string();
        assert!(error.contains("503"));
        assert!(!error.contains("secret"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(
            response(&client, &format!("http://{address}/?token=secret"), 1024)
                .await
                .unwrap_err_string()
                .contains("no automatic retry")
        );
        let short = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        assert!(response(&short, &format!("{}/stall", server.url), 1024)
            .await
            .unwrap_err_string()
            .contains("timeout"));
    }

    #[tokio::test]
    async fn fixture_download_verifies_native_platform_and_version_before_any_install() {
        let fixture = Fixture::new();
        let server = Server::new().await;
        let client = test_client();
        let native = fixture.binary();
        let payload = fs::read(&native).unwrap();
        let binary = server.asset("/binary", payload.clone());
        let digest: String = Sha256::digest(&payload)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let checksum_asset =
            server.asset("/binary.sha256", format!("{digest}  binary\n").into_bytes());
        let temporary = prepare(
            &client,
            &binary,
            &checksum_asset,
            &fixture.0,
            0o700,
            "0.14.0",
        )
        .await
        .unwrap();
        assert_eq!(executable_version(&temporary.0).await.unwrap(), "0.14.0");
        let staged = temporary.0.clone();
        drop(temporary);
        assert!(!staged.exists());
        assert!(prepare(
            &client,
            &binary,
            &checksum_asset,
            &fixture.0,
            0o700,
            "0.15.0"
        )
        .await
        .unwrap_err_string()
        .contains("wrong version"));
        let wrong = server.asset(
            "/binary",
            b"#!/bin/sh\necho muqun-gateway 0.14.0\n".to_vec(),
        );
        let digest: String = Sha256::digest(b"#!/bin/sh\necho muqun-gateway 0.14.0\n")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let sum = server.asset("/binary.sha256", format!("{digest}  binary\n").into_bytes());
        assert!(prepare(&client, &wrong, &sum, &fixture.0, 0o700, "0.14.0")
            .await
            .unwrap_err_string()
            .contains("platform"));
        assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".muqun-gateway.update-")));
    }

    #[tokio::test]
    async fn version_probe_bounds_output_and_time_without_shell_interpolation() {
        let fixture = Fixture::new();
        let excessive = fixture.write("version;not-a-shell-command", b"#!/bin/sh\nprintf 'muqun-gateway '\ni=0; while [ $i -lt 300 ]; do printf x; i=$((i+1)); done\n");
        assert!(executable_version(&excessive)
            .await
            .unwrap_err_string()
            .contains("probe failed"));
        let hung = fixture.write("hung", b"#!/bin/sh\nwhile :; do :; done\n");
        assert!(executable_version(&hung)
            .await
            .unwrap_err_string()
            .contains("timed out"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn version_spawn_retries_only_unexecuted_busy_inode_and_has_a_deadline() {
        let fixture = Fixture::new();
        let binary = fixture.binary();
        let writer = OpenOptions::new().write(true).open(&binary).unwrap();
        assert_eq!(
            std::process::Command::new(&binary)
                .arg("--version")
                .spawn()
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ETXTBSY)
        );
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(writer);
        });
        assert_eq!(executable_version(&binary).await.unwrap(), "0.14.0");
        release.join().unwrap();
        let held = OpenOptions::new().write(true).open(&binary).unwrap();
        let started = std::time::Instant::now();
        assert!(executable_version(&binary)
            .await
            .unwrap_err_string()
            .contains("cannot run side-effect-free"));
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(held);
    }

    #[tokio::test]
    async fn downloaded_start_failure_restores_exact_binary_and_real_isolated_gateway_state() {
        use base64::Engine as _;
        use serde_json::json;
        let fixture = Fixture::new();
        let home = fixture.0.join("home");
        let parent = home.join(".local/bin");
        let config = fixture.0.join("config");
        let state = fixture.0.join("state");
        let tools = fixture.0.join("tools");
        for dir in [&parent, &config, &state, &tools] {
            fs::create_dir_all(dir).unwrap();
        }
        for name in ["tmux", "tailscale", "systemctl", "launchctl"] {
            let path = tools.join(name);
            fs::write(
                &path,
                if name == "systemctl" {
                    &b"#!/bin/sh\necho 0\nexit 0\n"[..]
                } else {
                    &b"#!/bin/sh\nexit 1\n"[..]
                },
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // cargo test builds this CLI for the integration test targets too. Only
        // private copies are launched; no original executable is replaced.
        let cli = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("muqun-gateway");
        let exe = parent.join("muqun-gateway");
        fs::copy(&cli, &exe).expect("build the CLI before running this focused process test");
        let controller = fixture.0.join("muqun-gateway");
        fs::copy(&cli, &controller).unwrap();
        let old_hash = Sha256::digest(fs::read(&exe).unwrap());
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let token = "isolated-update-admin";
        fs::write(config.join("config.json"), serde_json::to_vec(&json!({
            "server_id": "isolated-update", "label": "update fixture", "listen": address.to_string(),
            "public_url": format!("http://{address}"), "token_hash": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(token)),
            "sessions": [{"id": "qa", "label": "isolated", "backend": "tmux", "socket_path": fixture.0.join("absent.sock")}],
            "autostart_backends": [], "opencode": {"enabled": false, "autostart": false}, "deepseek": {"enabled": false}, "t3": {"enabled": false}
        })).unwrap()).unwrap();
        fs::write(config.join("pairing.json"), serde_json::to_vec(&json!({"payload": {
            "kind": "muqun-gateway", "server_id": "isolated-update", "label": "test", "url": format!("http://{address}"), "token": token,
            "transport_key": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1_u8; 32])
        }})).unwrap()).unwrap();
        let identity = fs::read(config.join("pairing.json")).unwrap();
        let config_before = fs::read(config.join("config.json")).unwrap();
        let command = |program: &Path, action: &str| -> Result<()> {
            let output = std::process::Command::new(program)
                .arg(action)
                .env_clear()
                .current_dir(&fixture.0)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("MUQUN_GATEWAY_CONFIG_DIR", &config)
                .env("MUQUN_GATEWAY_STATE_DIR", &state)
                .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
                .env("LC_CTYPE", "C.UTF-8")
                .stdin(std::process::Stdio::null())
                .output()?;
            anyhow::ensure!(
                output.status.success(),
                "isolated CLI {action} failed ({}): {} {}",
                output.status,
                String::from_utf8_lossy(&output.stderr),
                fs::read_to_string(state.join("gateway.log")).unwrap_or_default()
            );
            Ok(())
        };
        struct Cleanup<F: Fn()>(F);
        impl<F: Fn()> Drop for Cleanup<F> {
            fn drop(&mut self) {
                (self.0)();
            }
        }
        let _cleanup = Cleanup(|| {
            let _ = command(&controller, "stop");
        });
        command(&exe, "start").unwrap();
        let pid_before = fs::read_to_string(state.join("gateway.pid")).unwrap();
        let server = Server::new().await;
        let native = fixture.binary();
        let payload = fs::read(native).unwrap();
        let hash: String = Sha256::digest(&payload)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let binary = server.asset("/binary", payload);
        let sum = server.asset("/binary.sha256", format!("{hash}  binary\n").into_bytes());
        let temporary = prepare(&test_client(), &binary, &sum, &parent, 0o700, "0.14.0")
            .await
            .unwrap();
        let mut calls = Vec::new();
        let error = install(&exe, &temporary.0, &parent.join(BACKUP), true, |action| {
            calls.push(action);
            // Like a live TUI controller, stop retains the old implementation;
            // start uses the atomically replaced destination, not the old image.
            command(
                if action == Action::Stop {
                    &controller
                } else {
                    &exe
                },
                action.verb(),
            )
        })
        .unwrap_err_string();
        let pid_after = fs::read_to_string(state.join("gateway.pid")).unwrap();
        command(&controller, "stop").unwrap(); // clean up before any assertions
        assert!(
            error.contains("exact previous binary and running state restored"),
            "{error}"
        );
        assert_eq!(
            calls,
            [Action::Stop, Action::Start, Action::Stop, Action::Start]
        );
        assert_ne!(pid_before, pid_after);
        assert_eq!(Sha256::digest(fs::read(&exe).unwrap()), old_hash);
        assert_eq!(fs::read(config.join("pairing.json")).unwrap(), identity);
        assert_eq!(fs::read(config.join("config.json")).unwrap(), config_before);
        assert!(!parent.join(BACKUP).exists());
    }
}
