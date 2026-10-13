//! Read-only incremental observation of one provider-selected rollout.
//!
//! The caller obtains the path from authenticated native thread metadata and
//! revalidates the exact terminal/conversation binding before publishing a batch. This
//! module never scans directories, resolves a thread by mtime, or exposes paths.
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

const MAX_HEADER: usize = 256 * 1024;
const MAX_LINE: usize = 4 * 1024 * 1024;
pub(crate) const MAX_BATCH_BYTES: usize = 256 * 1024;
const INITIAL_TAIL_BYTES: u64 = 1024 * 1024;
const ANCHOR_BYTES: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TailError {
    InvalidPath,
    Identity,
    Replaced,
    Truncated,
    Rewritten,
    Malformed,
    Oversized,
    Io,
    Fenced,
}

pub(crate) struct TailBatch {
    pub records: Vec<Value>,
    pub caught_up: bool,
}

/// A cursor is permanently fenced after any read/identity failure. Rebinding
/// requires an explicit authoritative history refresh, never an automatic replay.
pub(crate) struct RolloutTail {
    home: PathBuf,
    path: PathBuf,
    file: File,
    identity: (u64, u64),
    header: Vec<u8>,
    offset: u64,
    anchor: Vec<u8>,
    partial: Vec<u8>,
    skip_fragment: bool,
    fenced: bool,
}

impl RolloutTail {
    pub(crate) fn open(home: &Path, path: &Path, thread: &str) -> Result<Self, TailError> {
        if uuid::Uuid::parse_str(thread)
            .ok()
            .is_none_or(|id| id.hyphenated().to_string() != thread)
        {
            return Err(TailError::Identity);
        }
        let mut file = open_confined(home, path)?;
        let metadata = file.metadata().map_err(|_| TailError::Io)?;
        let mut header = Vec::new();
        BufReader::new((&mut file).take(MAX_HEADER as u64))
            .read_until(b'\n', &mut header)
            .map_err(|_| TailError::Io)?;
        if header.last() != Some(&b'\n') {
            return Err(TailError::Oversized);
        }
        let parsed: Value = serde_json::from_slice(&header).map_err(|_| TailError::Malformed)?;
        if parsed["type"] != "session_meta" || parsed["payload"]["id"] != thread {
            return Err(TailError::Identity);
        }
        let offset = metadata
            .len()
            .saturating_sub(INITIAL_TAIL_BYTES)
            .max(header.len() as u64);
        let skip_fragment =
            offset > header.len() as u64 && read_range(&mut file, offset - 1, 1)? != b"\n";
        let anchor = read_range(
            &mut file,
            offset.saturating_sub(ANCHOR_BYTES),
            offset.min(ANCHOR_BYTES) as usize,
        )?;
        Ok(Self {
            home: home.into(),
            path: path.into(),
            identity: file_identity(&metadata),
            file,
            header,
            offset,
            anchor,
            partial: Vec::new(),
            skip_fragment,
            fenced: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn poll(&mut self) -> Result<TailBatch, TailError> {
        if self.fenced {
            return Err(TailError::Fenced);
        }
        let result = self.poll_inner();
        if result.is_err() {
            self.fenced = true;
            self.partial.clear();
        }
        result
    }

    fn poll_inner(&mut self) -> Result<TailBatch, TailError> {
        // Reopen through confined directory descriptors. A renamed/replaced
        // pathname must not leave us silently reading the old unlinked inode.
        let mut current = open_confined(&self.home, &self.path)?;
        let metadata = current.metadata().map_err(|_| TailError::Io)?;
        if file_identity(&metadata) != self.identity {
            return Err(TailError::Replaced);
        }
        if metadata.len() < self.offset {
            return Err(TailError::Truncated);
        }
        if read_range(&mut current, 0, self.header.len())? != self.header
            || read_range(
                &mut current,
                self.offset - self.anchor.len() as u64,
                self.anchor.len(),
            )? != self.anchor
        {
            return Err(TailError::Rewritten);
        }
        let previous_offset = self.offset;
        let previous_anchor = self.anchor.clone();
        let count = (metadata.len() - self.offset).min(MAX_BATCH_BYTES as u64) as usize;
        let bytes = read_range(&mut self.file, self.offset, count)?;
        if bytes.len() != count {
            return Err(TailError::Truncated);
        }
        self.offset += bytes.len() as u64;
        let mut records = Vec::new();
        for fragment in bytes.split_inclusive(|byte| *byte == b'\n') {
            let complete = fragment.last() == Some(&b'\n');
            if self.skip_fragment {
                if complete {
                    self.skip_fragment = false;
                }
                continue;
            }
            if self.partial.len() + fragment.len() > MAX_LINE {
                return Err(TailError::Oversized);
            }
            self.partial.extend_from_slice(fragment);
            if complete {
                if !self.partial.iter().all(u8::is_ascii_whitespace) {
                    records.push(
                        serde_json::from_slice(&self.partial).map_err(|_| TailError::Malformed)?,
                    );
                }
                self.partial.clear();
            }
        }
        self.anchor = read_range(
            &mut self.file,
            self.offset.saturating_sub(ANCHOR_BYTES),
            self.offset.min(ANCHOR_BYTES) as usize,
        )?;
        // Check again after I/O, before returning any newly parsed rows. The
        // caller separately fences native-selected path/terminal identity changes.
        let mut after_file = open_confined(&self.home, &self.path)?;
        let after = after_file.metadata().map_err(|_| TailError::Io)?;
        if file_identity(&after) != self.identity {
            return Err(TailError::Replaced);
        }
        if after.len() < self.offset {
            return Err(TailError::Truncated);
        }
        if read_range(&mut after_file, 0, self.header.len())? != self.header
            || read_range(
                &mut after_file,
                previous_offset - previous_anchor.len() as u64,
                previous_anchor.len(),
            )? != previous_anchor
            || read_range(&mut after_file, previous_offset, count)? != bytes
        {
            return Err(TailError::Rewritten);
        }
        Ok(TailBatch {
            records,
            caught_up: self.offset == after.len() && self.partial.is_empty(),
        })
    }
}

fn read_range(file: &mut File, offset: u64, count: usize) -> Result<Vec<u8>, TailError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| TailError::Io)?;
    let mut bytes = Vec::with_capacity(count);
    file.take(count as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| TailError::Io)?;
    Ok(bytes)
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(unix)]
fn open_confined(home: &Path, path: &Path) -> Result<File, TailError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    if !home.is_absolute()
        || !path.is_absolute()
        || path.extension().is_none_or(|ext| ext != "jsonl")
    {
        return Err(TailError::InvalidPath);
    }
    let root = home.join("sessions");
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| TailError::InvalidPath)?;
    if relative.components().next().is_none()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(TailError::InvalidPath);
    }
    let mut parent = File::open("/").map_err(|_| TailError::Io)?;
    let components: Vec<_> = path
        .components()
        .filter(|c| *c != Component::RootDir)
        .collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(TailError::InvalidPath);
        };
        let name = CString::new(name.as_bytes()).map_err(|_| TailError::InvalidPath)?;
        let last = index + 1 == components.len();
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: parent is a live directory descriptor; name is NUL terminated;
        // the returned descriptor is owned exactly once and no write flag is used.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(TailError::InvalidPath);
        }
        // SAFETY: successful openat transferred a fresh descriptor to this scope.
        parent = unsafe { File::from_raw_fd(fd) };
    }
    if !parent.metadata().map_err(|_| TailError::Io)?.is_file() {
        return Err(TailError::InvalidPath);
    }
    Ok(parent)
}

#[cfg(not(unix))]
fn file_identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}
#[cfg(not(unix))]
fn open_confined(_: &Path, _: &Path) -> Result<File, TailError> {
    Err(TailError::InvalidPath)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    const ID: &str = "00000000-0000-4000-8000-000000000001";
    struct Fixture {
        home: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let home = std::env::temp_dir().join(format!("muqun-rollout-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(home.join("sessions/day")).unwrap();
            let home = home.canonicalize().unwrap();
            let path = home.join("sessions/day/rollout-fixture.jsonl");
            std::fs::write(
                &path,
                format!("{}\n", json!({"type":"session_meta","payload":{"id":ID}})),
            )
            .unwrap();
            Self { home, path }
        }
        fn append(&self, text: &str) {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        fn tail(&self) -> RolloutTail {
            RolloutTail::open(&self.home, &self.path, ID).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn incremental_poll_keeps_partial_lines_and_never_replays_unchanged_bytes() {
        let fixture = Fixture::new();
        fixture.append("{\"type\":\"event_msg\",\"payload\":");
        let mut tail = fixture.tail();
        assert!(tail.poll().unwrap().records.is_empty());
        let offset = tail.offset;
        assert!(!tail.poll().unwrap().caught_up);
        assert_eq!(tail.offset, offset);
        fixture.append("{\"type\":\"item_completed\"}}\n");
        let batch = tail.poll().unwrap();
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0]["payload"]["type"], "item_completed");
        assert!(batch.caught_up);
        assert!(tail.poll().unwrap().records.is_empty());
    }

    #[test]
    fn identity_path_symlinks_and_non_files_are_refused() {
        let fixture = Fixture::new();
        assert!(matches!(
            RolloutTail::open(
                &fixture.home,
                &fixture.path,
                "00000000-0000-4000-8000-000000000002"
            ),
            Err(TailError::Identity)
        ));
        let outside = fixture.home.join("outside.jsonl");
        std::fs::copy(&fixture.path, &outside).unwrap();
        assert!(matches!(
            RolloutTail::open(&fixture.home, &outside, ID),
            Err(TailError::InvalidPath)
        ));
        let symlink = fixture.home.join("sessions/link.jsonl");
        std::os::unix::fs::symlink(&fixture.path, &symlink).unwrap();
        assert!(matches!(
            RolloutTail::open(&fixture.home, &symlink, ID),
            Err(TailError::InvalidPath)
        ));
        let linked_dir = fixture.home.join("sessions/link");
        std::os::unix::fs::symlink(fixture.path.parent().unwrap(), &linked_dir).unwrap();
        assert!(matches!(
            RolloutTail::open(&fixture.home, &linked_dir.join("rollout-fixture.jsonl"), ID),
            Err(TailError::InvalidPath)
        ));
        assert!(matches!(
            RolloutTail::open(
                &fixture.home,
                &fixture
                    .home
                    .join("sessions/day/../day/rollout-fixture.jsonl"),
                ID
            ),
            Err(TailError::InvalidPath)
        ));
    }

    #[test]
    fn replacement_truncation_and_rewrite_fence_the_cursor() {
        for mutation in ["replace", "truncate", "rewrite"] {
            let fixture = Fixture::new();
            fixture.append("{\"n\":1}\n");
            let mut tail = fixture.tail();
            assert_eq!(tail.poll().unwrap().records.len(), 1);
            match mutation {
                "replace" => {
                    let other = fixture.path.with_extension("replacement");
                    std::fs::copy(&fixture.path, &other).unwrap();
                    std::fs::rename(other, &fixture.path).unwrap();
                }
                "truncate" => {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&fixture.path)
                        .unwrap()
                        .set_len(1)
                        .unwrap();
                }
                _ => {
                    let bytes = std::fs::read_to_string(&fixture.path)
                        .unwrap()
                        .replace("\"n\":1", "\"n\":2");
                    std::fs::write(&fixture.path, bytes).unwrap();
                }
            }
            let expected = match mutation {
                "replace" => TailError::Replaced,
                "truncate" => TailError::Truncated,
                _ => TailError::Rewritten,
            };
            assert_eq!(tail.poll().err(), Some(expected));
            assert_eq!(tail.poll().err(), Some(TailError::Fenced));
        }
    }

    #[test]
    fn initial_overlap_and_each_poll_are_bounded_with_boundary_fragments_skipped() {
        let fixture = Fixture::new();
        fixture.append(&format!(
            "{{\"padding\":\"{}\"}}\n{{\"new\":1}}\n",
            "x".repeat(INITIAL_TAIL_BYTES as usize + 100)
        ));
        let mut tail = fixture.tail();
        assert!(tail.offset > tail.header.len() as u64);
        let mut records = Vec::new();
        for _ in 0..5 {
            let before = tail.offset;
            let batch = tail.poll().unwrap();
            assert!(tail.offset - before <= MAX_BATCH_BYTES as u64);
            records.extend(batch.records);
            if batch.caught_up {
                break;
            }
        }
        assert_eq!(records, vec![json!({"new":1})]);
        assert!(tail.poll().unwrap().records.is_empty());
    }

    #[test]
    fn oversized_and_malformed_new_records_fail_without_unbounded_buffering() {
        for malformed in [true, false] {
            let fixture = Fixture::new();
            let mut tail = fixture.tail();
            fixture.append(if malformed {
                "broken\n"
            } else {
                "{\"large\":\""
            });
            if !malformed {
                fixture.append(&"x".repeat(MAX_LINE + 1));
            }
            let mut error = None;
            for _ in 0..20 {
                if let Err(reason) = tail.poll() {
                    error = Some(reason);
                    break;
                }
            }
            assert_eq!(
                error,
                Some(if malformed {
                    TailError::Malformed
                } else {
                    TailError::Oversized
                })
            );
            assert!(tail.partial.is_empty());
        }
    }
}
