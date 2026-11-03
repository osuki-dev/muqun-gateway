//! A cheap token for FX's saved-session store (`~/.fx/sessions`). While it
//! holds, listings and saved-session reads stay valid without an `fx acp`
//! process: the Home refresh and the task follower ask every minute, longer
//! than the metadata cache, and each cold child competes with the 30-second
//! listing budget.
use std::ffi::OsStr;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 16384;
/// Deep enough for `v2/<id>/log.jsonl` and a legacy `<id>/events.jsonl`.
const MAX_DEPTH: usize = 3;

pub(super) fn fingerprint() -> Option<u64> {
    fingerprint_in(&dirs::home_dir()?.join(".fx").join("sessions"))
}

/// Order-independent over every entry's path and, for a file, its length and
/// mtime. The entry set alone sees a session created, deleted or trashed, so
/// a folder's own mtime is not read: an fx start creates lock and temporary
/// files inside these folders without changing any conversation, and those
/// files are skipped too. Blob folders count but are not entered.
fn fingerprint_in(root: &Path) -> Option<u64> {
    let mut fingerprint = 0_u64;
    let mut entries = 0_usize;
    let mut pending: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) if depth == 0 && error.kind() == std::io::ErrorKind::NotFound => {
                return Some(0)
            }
            Err(_) => return None,
        };
        for entry in read {
            let entry = entry.ok()?;
            let name = entry.file_name();
            if skipped(&name) {
                continue;
            }
            entries += 1;
            if entries > MAX_ENTRIES {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            let mut hasher = DefaultHasher::new();
            entry.path().strip_prefix(root).ok()?.hash(&mut hasher);
            if metadata.is_dir() {
                true.hash(&mut hasher);
            } else {
                (false, metadata.len(), metadata.modified().ok()?).hash(&mut hasher);
            }
            fingerprint = fingerprint.wrapping_add(hasher.finish());
            if metadata.is_dir() && depth + 1 < MAX_DEPTH && name != "blobs" {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    Some(fingerprint)
}

fn skipped(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.ends_with(".lock") || name.contains(".tmp")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn append(root: &Path, relative: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join(relative))
            .unwrap();
        std::io::Write::write_all(&mut file, b"{}\n").unwrap();
    }

    #[test]
    fn store_fingerprint_moves_only_when_a_saved_conversation_changes() {
        let scratch =
            Scratch(std::env::temp_dir().join(format!("muqun-fx-store-{}", uuid::Uuid::new_v4())));
        let root = &scratch.0;
        assert_eq!(fingerprint_in(&root.join("absent")), Some(0));
        // The layout of a real store: a legacy session folder, the v2 store
        // with its index and one session, and fx's own lock and tmp files.
        for (relative, bytes) in [
            (
                "index.json",
                br#"{"schema_version":3,"sessions":[]}"#.as_slice(),
            ),
            ("latest.lock", b""),
            (
                "1787376884632-legacy/events.jsonl",
                b"{\"type\":\"user\"}\n",
            ),
            ("1787376884632-legacy/session.lock", b""),
            (
                "v2/index.jsonl",
                b"{\"op\":\"put\",\"id\":\"e267Rh3iRkcR\"}\n",
            ),
            ("v2/index.lock", b""),
            ("v2/e267Rh3iRkcR/log.jsonl", b"{\"seq\":1}\n"),
            ("v2/e267Rh3iRkcR/lock", b""),
            ("v2/e267Rh3iRkcR/blobs/one.bin", b"blob"),
        ] {
            write(root, relative, bytes);
        }
        let initial = fingerprint_in(root).unwrap();
        assert_eq!(fingerprint_in(root), Some(initial), "re-reads agree");
        // (what fx does, the token moves)
        let steps: [(&str, &dyn Fn(), bool); 7] = [
            (
                "fx starts: locks and a tmp index",
                &|| {
                    write(root, "latest.lock", b"");
                    write(root, "v2/index.jsonl.tmp", b"partial");
                    write(root, "v2/.tmp/x", b"");
                },
                false,
            ),
            (
                "a turn appends to a v2 conversation",
                &|| append(root, "v2/e267Rh3iRkcR/log.jsonl"),
                true,
            ),
            (
                "the v2 index gains a session",
                &|| append(root, "v2/index.jsonl"),
                true,
            ),
            (
                "a new v2 session folder",
                &|| write(root, "v2/gH0fOOzfQ5XK/log.jsonl", b"{\"seq\":1}\n"),
                true,
            ),
            (
                "a legacy conversation grows",
                &|| append(root, "1787376884632-legacy/events.jsonl"),
                true,
            ),
            (
                "a session is deleted",
                &|| std::fs::remove_dir_all(root.join("v2/gH0fOOzfQ5XK")).unwrap(),
                true,
            ),
            (
                "a legacy folder is trashed",
                &|| {
                    std::fs::create_dir_all(root.join("trash+v1")).unwrap();
                    std::fs::rename(
                        root.join("1787376884632-legacy"),
                        root.join("trash+v1/1787376884632-legacy"),
                    )
                    .unwrap()
                },
                true,
            ),
        ];
        let mut previous = initial;
        for (what, change, moves) in steps {
            std::thread::sleep(std::time::Duration::from_millis(5));
            change();
            let next = fingerprint_in(root).unwrap();
            assert_eq!(next != previous, moves, "{what}");
            previous = next;
        }
        // Over budget stays unknown rather than claiming an unchanged store.
        for n in 0..=MAX_ENTRIES {
            write(root, &format!("v2/flood/{n}"), b"");
        }
        assert_eq!(fingerprint_in(root), None);
    }
}
