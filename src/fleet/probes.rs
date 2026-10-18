//! Toolchain probes: a fixed table of programs asked for their version, each
//! under a short timeout. A probe that fails or times out is simply omitted.

use std::path::PathBuf;
use std::time::Duration;

use super::manifest::Toolchain;
use crate::agents::tasks::find_on_path;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct ProbeSpec {
    pub id: &'static str,
    /// Program followed by its arguments. Empty for `android-sdk`, which is a
    /// directory check rather than a program.
    pub argv: &'static [&'static str],
    pub version_from: fn(&str) -> Option<String>,
}

pub(crate) const PROBES: &[ProbeSpec] = &[
    ProbeSpec {
        id: "xcode",
        argv: &["xcodebuild", "-version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "android-sdk",
        argv: &[],
        version_from: highest_platform,
    },
    ProbeSpec {
        id: "adb",
        argv: &["adb", "version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "node",
        argv: &["node", "--version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "bun",
        argv: &["bun", "--version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "rust",
        argv: &["cargo", "--version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "go",
        argv: &["go", "version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "python3",
        argv: &["python3", "--version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "docker",
        argv: &["docker", "--version"],
        version_from: parse_version,
    },
    ProbeSpec {
        id: "git",
        argv: &["git", "--version"],
        version_from: parse_version,
    },
];

/// The first dotted version number in a program's version output:
/// `v22.3.0` -> `22.3.0`, `go version go1.23.1 linux/amd64` -> `1.23.1`.
pub(crate) fn parse_version(output: &str) -> Option<String> {
    for word in output.split_whitespace() {
        let Some(start) = word.find(|c: char| c.is_ascii_digit()) else {
            continue;
        };
        let candidate: String = word[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let candidate = candidate.trim_end_matches('.');
        if candidate.contains('.') {
            return Some(candidate.to_string());
        }
    }
    None
}

/// The highest `android-N` in a listing of an SDK's `platforms/` directory.
fn highest_platform(listing: &str) -> Option<String> {
    listing
        .lines()
        .filter_map(|line| line.trim().strip_prefix("android-")?.parse::<u32>().ok())
        .max()
        .map(|level| level.to_string())
}

async fn run_command(spec: &ProbeSpec) -> Option<Toolchain> {
    let (program, args) = spec.argv.split_first()?;
    let path = find_on_path(program)?;
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new(&path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    // Some print the version on stderr (`adb`, old `python3`).
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(Toolchain {
        id: spec.id.to_string(),
        version: (spec.version_from)(&text),
        path: Some(path.to_string_lossy().into_owned()),
    })
}

async fn android_sdk(spec: &ProbeSpec) -> Option<Toolchain> {
    let root = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|dir| dir.is_dir())?;
    let mut listing = String::new();
    if let Ok(mut entries) = tokio::fs::read_dir(root.join("platforms")).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            listing.push_str(&entry.file_name().to_string_lossy());
            listing.push('\n');
        }
    }
    Some(Toolchain {
        id: spec.id.to_string(),
        version: (spec.version_from)(&listing),
        path: Some(root.to_string_lossy().into_owned()),
    })
}

async fn probe(spec: &ProbeSpec) -> Option<Toolchain> {
    if spec.argv.is_empty() {
        tokio::time::timeout(PROBE_TIMEOUT, android_sdk(spec))
            .await
            .ok()?
    } else {
        run_command(spec).await
    }
}

/// Every probe that succeeded, in table order.
pub(crate) async fn run() -> Vec<Toolchain> {
    futures::future::join_all(PROBES.iter().map(probe))
        .await
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_real_probe_output() {
        let cases = [
            ("node v26.5.0\n", Some("26.5.0")),
            ("v22.3.0\n", Some("22.3.0")),
            ("cargo 1.97.1 (abc 2026-01-01)\n", Some("1.97.1")),
            ("go version go1.23.1 linux/amd64\n", Some("1.23.1")),
            ("Python 3.13.1\n", Some("3.13.1")),
            ("Docker version 28.1.1, build 4eba377\n", Some("28.1.1")),
            ("Xcode 16.2\nBuild version 16C5032a\n", Some("16.2")),
            (
                "Android Debug Bridge version 1.0.41\nVersion 35.0.2\n",
                Some("1.0.41"),
            ),
            ("no digits here", None),
            ("build 7", None),
        ];
        for (output, want) in cases {
            assert_eq!(parse_version(output).as_deref(), want, "{output:?}");
        }
    }

    #[test]
    fn the_android_sdk_version_is_its_highest_platform() {
        assert_eq!(
            highest_platform("android-9\nandroid-34\nandroid-35\nREADME\n").as_deref(),
            Some("35")
        );
        assert_eq!(highest_platform(""), None);
    }
}
