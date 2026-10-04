//! Directory names as the App types them: `~` expansion for every agent route
//! that takes a directory.

use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
