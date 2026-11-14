//! In-app update check and atomic binary replacement for Muqun Desktop.

use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const REPO: &str = "osuki-dev/muqun-gateway";
const USER_AGENT: &str = concat!("muqun-desktop/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub download_url: String,
    pub checksum_url: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

fn platform_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        .context("cannot initialize HTTP client for update check")
}

/// Query GitHub Releases to check if a newer version of Muqun Desktop is available.
pub async fn check_latest(include_preview: bool) -> Result<Option<UpdateInfo>> {
    let client = client()?;
    let url = if include_preview {
        format!("https://api.github.com/repos/{REPO}/releases")
    } else {
        format!("https://api.github.com/repos/{REPO}/releases/latest")
    };

    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .context("failed to fetch release info from GitHub")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned HTTP {}", response.status());
    }

    let current = env!("CARGO_PKG_VERSION");
    let current_clean = current.trim_start_matches('v');

    let releases: Vec<GitHubRelease> = if include_preview {
        response
            .json()
            .await
            .context("failed to parse releases list")?
    } else {
        let single: GitHubRelease = response.json().await.context("failed to parse release")?;
        vec![single]
    };

    let target = platform_target().context("unsupported platform for desktop self-update")?;

    for release in releases {
        if release.draft {
            continue;
        }
        if release.prerelease && !include_preview {
            continue;
        }

        let tag = release.tag_name.trim_start_matches('v');
        // Simple semver check: if tag is newer than current
        if is_newer(tag, current_clean) {
            // Find binary asset matching desktop and platform target
            let mut download_url = None;
            let mut checksum_url = None;

            for asset in &release.assets {
                let name = &asset.name;
                // Matches e.g. muqun-desktop-x86_64-unknown-linux-musl or muqun-desktop-Linux
                let matches_target = name.contains(target)
                    || (target.contains("darwin")
                        && (name.contains("macOS") || name.contains("darwin")))
                    || (target.contains("linux")
                        && (name.contains("Linux") || name.contains("linux")));

                if name.starts_with("muqun-desktop") && matches_target {
                    if name.ends_with(".sha256") {
                        checksum_url = Some(asset.browser_download_url.clone());
                    } else if !name.ends_with(".tar.gz") && !name.ends_with(".zip") {
                        download_url = Some(asset.browser_download_url.clone());
                    }
                }
            }

            if let Some(url) = download_url {
                return Ok(Some(UpdateInfo {
                    current_version: current.to_string(),
                    latest_version: release.tag_name,
                    download_url: url,
                    checksum_url,
                }));
            }
        }
    }

    Ok(None)
}

/// Download binary from `info.download_url` and replace the current executable.
pub async fn download_and_install(info: &UpdateInfo) -> Result<()> {
    let client = client()?;
    let current_exe =
        std::env::current_exe().context("cannot determine current executable path")?;

    // Download binary
    let resp = client
        .get(&info.download_url)
        .send()
        .await
        .context("failed to download update binary")?;

    if !resp.status().is_success() {
        anyhow::bail!("failed to download binary: HTTP {}", resp.status());
    }

    let bytes = resp.bytes().await.context("failed to read binary bytes")?;

    // Verify checksum if available
    if let Some(checksum_url) = &info.checksum_url {
        if let Ok(chk_resp) = client.get(checksum_url).send().await {
            if chk_resp.status().is_success() {
                if let Ok(chk_text) = chk_resp.text().await {
                    let expected_sha = chk_text
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_lowercase();
                    if !expected_sha.is_empty() {
                        let mut hasher = Sha256::new();
                        hasher.update(&bytes);
                        let calculated_sha = hasher
                            .finalize()
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>();
                        anyhow::ensure!(
                            calculated_sha == expected_sha,
                            "checksum mismatch: expected {expected_sha}, got {calculated_sha}"
                        );
                    }
                }
            }
        }
    }

    // Write to a temporary file next to current_exe
    let exe_dir = current_exe
        .parent()
        .context("current_exe has no parent dir")?;
    let temp_path = exe_dir.join(format!(".muqun-desktop-update-{}", std::process::id()));

    std::fs::write(&temp_path, &bytes)
        .context("failed to write update binary to temporary file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&temp_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&temp_path, perms)?;
    }

    // Atomic rename over current executable
    #[cfg(unix)]
    {
        std::fs::rename(&temp_path, &current_exe)
            .context("failed to atomically replace running executable with updated binary")?;
    }

    #[cfg(windows)]
    {
        let backup = exe_dir.join(".muqun-desktop.old");
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::rename(&current_exe, &backup);
        std::fs::rename(&temp_path, &current_exe)
            .context("failed to replace running executable")?;
    }

    Ok(())
}

fn is_newer(tag: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<(u64, u64, u64, u64)> {
        let (base, preview) = match s.split_once("-preview.") {
            Some((b, p)) => (b, p.parse::<u64>().unwrap_or(0)),
            None => (s, u64::MAX),
        };
        let mut parts = base.split('.');
        let major = parts.next()?.parse::<u64>().ok()?;
        let minor = parts.next()?.parse::<u64>().ok()?;
        let patch = parts.next()?.parse::<u64>().ok()?;
        Some((major, minor, patch, preview))
    };

    match (parse(tag), parse(current)) {
        (Some(t), Some(c)) => t > c,
        _ => tag != current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.14.0-preview.2", "0.14.0-preview.1"));
        assert!(is_newer("0.14.0", "0.14.0-preview.1"));
        assert!(!is_newer("0.14.0-preview.1", "0.14.0"));
    }
}
