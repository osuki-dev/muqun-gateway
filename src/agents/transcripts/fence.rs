//! The path fence every transcript reader goes through.
//!
//! A reader names its root and proposes a file; nothing is opened unless the
//! proposal is a regular file, reached without a symlink at its last
//! component, whose canonical path lies inside the canonical root. The fence
//! only ever opens for reading.

use std::fs::File;
use std::path::{Path, PathBuf};

/// How many directory entries a fallback scan may look at before giving up.
const MAX_SCAN_ENTRIES: usize = 20_000;

/// The canonical form of a reader's root, or `None` when it does not exist
/// (an agent that never ran on this machine).
pub(crate) fn canonical_root(root: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    root.is_dir().then_some(root)
}

/// `candidate` if it is a regular, non-symlink file inside `root` (already
/// canonical), as its canonical path.
pub(crate) fn admit(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let link = std::fs::symlink_metadata(candidate).ok()?;
    if !link.file_type().is_file() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    (canonical.starts_with(root) && canonical != root).then_some(canonical)
}

/// Open an admitted file read-only, refusing a symlink swapped in since it
/// was admitted, and re-checking that what was opened is a regular file.
pub(crate) fn open(root: &Path, path: &Path) -> Option<File> {
    let path = admit(root, path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(&path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

/// A file named `file_name` directly inside one of `root`'s child
/// directories (`root/*/file_name`), for a reader that keeps one directory per
/// project and was not told which. When several match, the most recently
/// modified one wins. Bounded: a root with more entries than the cap is not
/// searched to the end.
pub(crate) fn find_in_children(root: &Path, file_name: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(root).ok()?.take(MAX_SCAN_ENTRIES) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(path) = admit(root, &entry.path().join(file_name)) else {
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(newest, _)| modified > *newest) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("transcript-fence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("project")).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn only_regular_files_inside_the_root_are_admitted() {
        let root = temp_root();
        let inside = root.join("project/a.jsonl");
        std::fs::write(&inside, "{}\n").unwrap();
        assert_eq!(admit(&root, &inside), Some(inside.clone()));
        assert!(open(&root, &inside).is_some());

        // A directory, a missing file and a path that climbs out are refused.
        assert_eq!(admit(&root, &root.join("project")), None);
        assert_eq!(admit(&root, &root.join("project/missing.jsonl")), None);
        let outside_dir = temp_root();
        let outside = outside_dir.join("project/b.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        assert!(outside.exists());
        let climbing = root
            .join("project/../..")
            .join(outside_dir.file_name().unwrap())
            .join("project/b.jsonl");
        assert_eq!(admit(&root, &climbing), None);
        let _ = std::fs::remove_dir_all(&outside_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_out_of_the_root() {
        let root = temp_root();
        let outside_dir = temp_root();
        let secret = outside_dir.join("project/secret.jsonl");
        std::fs::write(&secret, "{}\n").unwrap();

        // A symlinked file, even one pointing inside the root, is refused.
        let link = root.join("project/link.jsonl");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert_eq!(admit(&root, &link), None);
        assert!(open(&root, &link).is_none());

        // A symlinked project directory leading outside is refused too.
        let escape = root.join("escape");
        std::os::unix::fs::symlink(outside_dir.join("project"), &escape).unwrap();
        assert_eq!(admit(&root, &escape.join("secret.jsonl")), None);
        assert_eq!(find_in_children(&root, "secret.jsonl"), None);

        let _ = std::fs::remove_dir_all(&outside_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_child_scan_finds_the_file_in_any_project() {
        let root = temp_root();
        std::fs::create_dir_all(root.join("other")).unwrap();
        let file = root.join("other/id.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        assert_eq!(find_in_children(&root, "id.jsonl"), Some(file));
        assert_eq!(find_in_children(&root, "absent.jsonl"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
