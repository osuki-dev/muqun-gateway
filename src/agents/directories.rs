//! Directory names as the App types them: `~` expansion for every agent route
//! that takes a directory, and the `GET /api/agent-directories` completer.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// `~` and `~/…` against `home`; anything else unchanged. `~user` is not
/// supported and answers `None`, as does a `~` path with no home to expand to.
fn expand_home_in(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let Some(rest) = raw.strip_prefix('~') else {
        return Some(PathBuf::from(raw));
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    let home = home?;
    Some(if rest.is_empty() {
        home.to_path_buf()
    } else {
        home.join(rest)
    })
}

/// `~` and `~/…` → the home directory joined; anything else unchanged.
/// `~user` answers `None`.
pub(crate) fn expand_home(raw: &str) -> Option<PathBuf> {
    expand_home_in(raw, dirs::home_dir().as_deref())
}

/// A `directory` parameter as the agent should see it: trimmed, `~` expanded
/// and, when it names an existing absolute directory, canonical -- so
/// `~/Work/x` and `/home/u/Work/x` are one project, not two. A blank value
/// stays as it was, and so does one `expand_home` refuses (`~user`) or that is
/// relative, so it fails exactly as it did before; a missing directory comes
/// back expanded, for the caller's own not-found answer to name.
pub(crate) fn expand_directory_param(directory: Option<&str>) -> Option<String> {
    let directory = directory?;
    let trimmed = directory.trim();
    if trimmed.is_empty() {
        return Some(directory.to_string());
    }
    let Some(path) = expand_home(trimmed).filter(|p| p.is_absolute()) else {
        return Some(trimmed.to_string());
    };
    let path = match std::fs::canonicalize(&path) {
        Ok(canonical) if canonical.is_dir() => canonical,
        _ => path,
    };
    Some(path.to_string_lossy().into_owned())
}

/// Directories the completer never offers: dependency and build output, which
/// nobody opens as a workspace and which can be enormous.
const SKIPPED: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "build",
    "dist",
    ".cache",
    "__pycache__",
];
pub(crate) const MAX_RESULTS: usize = 30;
const MAX_ENTRIES_READ: usize = 2000;
const READ_BUDGET: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Completion {
    pub directories: Vec<DirectoryEntry>,
    pub truncated: bool,
}

/// Where to look and what to match: `~/Work/mu` → (`~/Work`, `mu`), a trailing
/// `/` an empty partial, a bare `~` the home directory itself. A prefix that is
/// neither `~`-rooted nor absolute has nowhere to look.
fn split_prefix(prefix: &str, home: Option<&Path>) -> Option<(PathBuf, String)> {
    let prefix = if prefix.is_empty() { "~" } else { prefix };
    if prefix == "~" {
        return Some((home?.to_path_buf(), String::new()));
    }
    if !prefix.starts_with("~/") && !prefix.starts_with('/') {
        return None;
    }
    let cut = prefix.rfind('/')? + 1;
    let (dir, partial) = prefix.split_at(cut);
    Some((expand_home_in(dir, home)?, partial.to_string()))
}

/// Complete `prefix` to the directories one level under it whose names start
/// with its last segment. Bounded: at most [`MAX_RESULTS`] answers from at most
/// 2000 entries read in at most 50 ms, `truncated` saying any bound was hit.
/// Symlinks are listed when they point at a directory and never read into.
pub(crate) async fn complete(prefix: &str, home: Option<&Path>) -> Completion {
    let Some((dir, partial)) = split_prefix(prefix, home) else {
        return Completion::default();
    };
    let deadline = tokio::time::Instant::now() + READ_BUDGET;
    let mut completion = Completion::default();

    let Ok(Ok(dir)) = tokio::time::timeout_at(deadline, tokio::fs::canonicalize(&dir)).await else {
        return completion;
    };
    let Ok(Ok(mut entries)) = tokio::time::timeout_at(deadline, tokio::fs::read_dir(&dir)).await
    else {
        return completion;
    };

    let partial = partial.to_lowercase();
    let want_dot = partial.starts_with('.');
    let mut read = 0usize;
    loop {
        if read >= MAX_ENTRIES_READ {
            completion.truncated = true;
            break;
        }
        let entry = match tokio::time::timeout_at(deadline, entries.next_entry()).await {
            Ok(Ok(Some(entry))) => entry,
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Err(_) => {
                completion.truncated = true;
                break;
            }
        };
        read += 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.starts_with('.') && !want_dot)
            || SKIPPED.contains(&name.as_str())
            || !name.to_lowercase().starts_with(&partial)
        {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        let is_dir = if file_type.is_symlink() {
            // One stat of the target to know it is a directory; never a read.
            matches!(
                tokio::time::timeout_at(deadline, tokio::fs::metadata(entry.path())).await,
                Ok(Ok(meta)) if meta.is_dir()
            )
        } else {
            file_type.is_dir()
        };
        if is_dir {
            completion.directories.push(DirectoryEntry {
                path: entry.path().to_string_lossy().into_owned(),
                name,
            });
        }
    }

    completion.directories.sort_by(|a, b| a.name.cmp(&b.name));
    if completion.directories.len() > MAX_RESULTS {
        completion.directories.truncate(MAX_RESULTS);
        completion.truncated = true;
    }
    completion
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muqun-dirs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn names(c: &Completion) -> Vec<&str> {
        c.directories.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn expands_tilde_forms_only() {
        let home = Path::new("/home/u");
        assert_eq!(expand_home_in("~", Some(home)), Some(home.to_path_buf()));
        assert_eq!(expand_home_in("~/", Some(home)), Some(home.to_path_buf()));
        assert_eq!(
            expand_home_in("~/Work/x", Some(home)),
            Some(PathBuf::from("/home/u/Work/x"))
        );
        assert_eq!(expand_home_in("~nobody", Some(home)), None);
        assert_eq!(expand_home_in("~nobody/x", Some(home)), None);
        assert_eq!(expand_home_in("~", None), None);
        assert_eq!(
            expand_home_in("rel/x", Some(home)),
            Some(PathBuf::from("rel/x"))
        );
        assert_eq!(
            expand_home_in("/abs", Some(home)),
            Some(PathBuf::from("/abs"))
        );
    }

    #[test]
    fn directory_param_leaves_what_it_cannot_expand() {
        let home = dirs::home_dir().unwrap();
        let missing = "~/muqun-no-such-dir-for-tests/sub";
        assert_eq!(
            expand_directory_param(Some(&format!(" {missing} "))),
            Some(
                home.join("muqun-no-such-dir-for-tests/sub")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(
            expand_directory_param(Some("~")),
            Some(home.canonicalize().unwrap().to_string_lossy().into_owned())
        );
        // An existing directory comes back canonical, `~` or not.
        let tmp = std::env::temp_dir();
        let canonical = tmp.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            expand_directory_param(Some(&format!("{}/.", tmp.display()))),
            Some(canonical)
        );
        assert_eq!(
            expand_directory_param(Some("~nobody")),
            Some("~nobody".into())
        );
        assert_eq!(expand_directory_param(Some("rel")), Some("rel".into()));
        assert_eq!(expand_directory_param(Some("")), Some("".into()));
        assert_eq!(expand_directory_param(None), None);
    }

    #[test]
    fn splits_prefix_at_the_last_slash() {
        let home = Path::new("/h");
        let split = |p: &str| split_prefix(p, Some(home));
        assert_eq!(
            split("~/Work/mu"),
            Some((PathBuf::from("/h/Work/"), "mu".into()))
        );
        assert_eq!(
            split("~/Work/"),
            Some((PathBuf::from("/h/Work/"), "".into()))
        );
        assert_eq!(split("~"), Some((PathBuf::from("/h"), "".into())));
        assert_eq!(split(""), Some((PathBuf::from("/h"), "".into())));
        assert_eq!(split("~/"), Some((PathBuf::from("/h"), "".into())));
        assert_eq!(split("/ho"), Some((PathBuf::from("/"), "ho".into())));
        assert_eq!(split("Work"), None);
        assert_eq!(split("~nobody/x"), None);
    }

    #[tokio::test]
    async fn filters_by_partial_case_insensitively_and_hides_dots() {
        let root = scratch("partial");
        for d in [
            "Muqun",
            "music",
            "other",
            ".mutt",
            ".config",
            "node_modules",
            ".cache",
        ] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        std::fs::write(root.join("mufile"), "").unwrap();
        let home = Some(root.as_path());

        let c = complete("~/mu", home).await;
        assert_eq!(names(&c), vec!["Muqun", "music"]);
        assert!(!c.truncated);
        assert_eq!(c.directories[0].path, root.join("Muqun").to_string_lossy());

        // A trailing slash lists everything visible, minus the skip list.
        let c = complete("~/", home).await;
        assert_eq!(names(&c), vec!["Muqun", "music", "other"]);

        // Dot directories only when asked for; `.cache` never.
        let c = complete("~/.c", home).await;
        assert_eq!(names(&c), vec![".config"]);

        // Absolute prefixes work the same way.
        let c = complete(&format!("{}/o", root.display()), None).await;
        assert_eq!(names(&c), vec!["other"]);
    }

    #[tokio::test]
    async fn caps_at_thirty_and_says_so() {
        let root = scratch("cap");
        for i in 0..40 {
            std::fs::create_dir(root.join(format!("d{i:02}"))).unwrap();
        }
        let c = complete("~/d", Some(root.as_path())).await;
        assert_eq!(c.directories.len(), MAX_RESULTS);
        assert_eq!(c.directories[0].name, "d00");
        assert!(c.truncated);
    }

    #[tokio::test]
    async fn missing_or_relative_answers_nothing() {
        let root = scratch("missing");
        let home = Some(root.as_path());
        assert_eq!(complete("~/nope/", home).await, Completion::default());
        assert_eq!(complete("~/nope/x", home).await, Completion::default());
        assert_eq!(complete("relative/x", home).await, Completion::default());
        assert_eq!(complete("~nobody/", home).await, Completion::default());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lists_a_symlinked_directory_without_following_files() {
        let root = scratch("link");
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("file"), "").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(root.join("file"), root.join("linkfile")).unwrap();
        let c = complete("~/", Some(root.as_path())).await;
        assert_eq!(names(&c), vec!["linkdir", "real"]);
        assert_eq!(
            c.directories[0].path,
            root.join("linkdir").to_string_lossy()
        );
    }

    #[tokio::test]
    async fn filesystem_root_is_listed() {
        let c = complete("/", None).await;
        assert!(!c.directories.is_empty());
        assert!(c.directories.iter().all(|d| !d.name.starts_with('.')));
    }
}
