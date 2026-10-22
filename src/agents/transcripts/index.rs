//! Agent-agnostic line-offset index over an append-only JSONL transcript,
//! with tail paging, stable cursors and a cheap `head` probe.
//!
//! A transcript can be hundreds of megabytes, so no request reads the whole
//! file. The first request for a file indexes it once (the start offset of
//! every complete line, which lines are visible, how they group, and which
//! line answers which key); later requests only index the bytes appended
//! since. Pages are then read line by line from those offsets, newest first.
//!
//! Line numbers never change while the file only grows, so a cursor is a line
//! number bound to the file's identity (device and inode). A file that is
//! replaced or shrinks is indexed afresh and its old cursors are gone.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use super::{fence, AnswerLookup, LineClass, SourceLine, TranscriptReader};
use crate::agents::domain::TimelineItem;

/// A line longer than this is counted as malformed and never parsed. Claude
/// Code inlines pasted images as base64, so single lines reach megabytes.
pub(crate) const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
/// Raw line bytes one page reads at most; a page always holds one line.
const MAX_PAGE_BYTES: u64 = 24 * 1024 * 1024;
/// Indexes kept in memory, least recently used evicted first. An index costs
/// about 16 bytes per line plus its answer keys.
const MAX_CACHED_INDEXES: usize = 8;
const READ_CHUNK: usize = 1024 * 1024;

pub(crate) const DEFAULT_PAGE_LIMIT: usize = 50;
pub(crate) const MAX_PAGE_LIMIT: usize = 200;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// No transcript for this session inside the reader's root.
    NotFound,
    /// The cursor is not one this endpoint issues.
    InvalidCursor,
    /// The cursor belongs to a file that has since been replaced or truncated.
    CursorGone,
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Head {
    /// Complete lines indexed. A line still being written is not counted.
    pub lines: u64,
    /// Visible lines: the ones pages are made of.
    pub entries: u64,
    /// The file's size, including any incomplete last line.
    pub bytes: u64,
    pub mtime_ms: u64,
    /// Lines that could not be classified (invalid JSON, oversized).
    pub malformed_lines: u64,
}

#[derive(Debug)]
pub(crate) struct Page {
    pub items: Vec<TimelineItem>,
    pub has_more: bool,
    pub next_before: Option<String>,
    pub head: Head,
    /// Visible lines on this page that could not be projected.
    pub skipped_lines: u64,
}

type Identity = (u64, u64);

#[derive(Default)]
struct LineIndex {
    identity: Identity,
    /// The offset just past the last complete line.
    indexed_bytes: u64,
    starts: Vec<u64>,
    visible: Vec<u32>,
    /// `group_start[i]` is the line that opened `visible[i]`'s group.
    group_start: Vec<u32>,
    answers: HashMap<String, u32>,
    malformed: u64,
    last_group: Option<(String, u32)>,
}

impl LineIndex {
    fn line_end(&self, line: usize) -> u64 {
        self.starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.indexed_bytes)
    }

    fn push(&mut self, reader: &dyn TranscriptReader, start: u64, line: Option<&[u8]>) {
        let number = self.starts.len() as u32;
        self.starts.push(start);
        let Some(line) = line.map(trim) else {
            self.malformed += 1;
            return;
        };
        if line.is_empty() {
            return;
        }
        let Some(LineClass {
            visible,
            group,
            answers,
        }) = reader.classify(line)
        else {
            self.malformed += 1;
            return;
        };
        for key in answers {
            self.answers.insert(key, number);
        }
        if !visible {
            return;
        }
        let start = match (&group, &self.last_group) {
            (Some(group), Some((last, start))) if group == last => *start,
            _ => number,
        };
        self.last_group = group.map(|group| (group, start));
        self.visible.push(number);
        self.group_start.push(start);
    }

    /// Index every complete line between `indexed_bytes` and `size`.
    fn extend(&mut self, reader: &dyn TranscriptReader, file: &mut File, size: u64) -> bool {
        if file.seek(SeekFrom::Start(self.indexed_bytes)).is_err() {
            return false;
        }
        let mut remaining = size - self.indexed_bytes;
        let mut chunk = vec![0_u8; READ_CHUNK];
        let mut pending: Vec<u8> = Vec::new();
        let mut oversized = false;
        let mut line_start = self.indexed_bytes;
        let mut offset = self.indexed_bytes;
        while remaining > 0 {
            let want = remaining.min(READ_CHUNK as u64) as usize;
            let read = match file.read(&mut chunk[..want]) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => return false,
            };
            remaining -= read as u64;
            let mut from = 0;
            while let Some(at) = chunk[from..read].iter().position(|byte| *byte == b'\n') {
                let piece = &chunk[from..from + at];
                if !oversized && pending.len() + piece.len() <= MAX_LINE_BYTES {
                    pending.extend_from_slice(piece);
                } else {
                    oversized = true;
                }
                let end = offset + (from + at) as u64 + 1;
                self.push(reader, line_start, (!oversized).then_some(&pending[..]));
                pending.clear();
                oversized = false;
                line_start = end;
                self.indexed_bytes = end;
                from += at + 1;
            }
            let rest = &chunk[from..read];
            if !oversized && pending.len() + rest.len() <= MAX_LINE_BYTES {
                pending.extend_from_slice(rest);
            } else {
                oversized = true;
                pending.clear();
            }
            offset += read as u64;
        }
        true
    }

    fn head(&self, size: u64, mtime_ms: u64) -> Head {
        Head {
            lines: self.starts.len() as u64,
            entries: self.visible.len() as u64,
            bytes: size,
            mtime_ms,
            malformed_lines: self.malformed,
        }
    }
}

fn trim(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let start = line
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(line.len());
    &line[start..]
}

struct Cached {
    path: PathBuf,
    index: Arc<Mutex<LineIndex>>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: Vec<Cached>,
    tick: u64,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

fn cached_index(path: &Path) -> Arc<Mutex<LineIndex>> {
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    cache.tick += 1;
    let tick = cache.tick;
    if let Some(entry) = cache.entries.iter_mut().find(|entry| entry.path == path) {
        entry.used = tick;
        return entry.index.clone();
    }
    if cache.entries.len() >= MAX_CACHED_INDEXES {
        if let Some(oldest) = cache
            .entries
            .iter()
            .enumerate()
            .min_by_key(|(_, entry)| entry.used)
            .map(|(at, _)| at)
        {
            cache.entries.swap_remove(oldest);
        }
    }
    let index = Arc::new(Mutex::new(LineIndex::default()));
    cache.entries.push(Cached {
        path: path.to_path_buf(),
        index: index.clone(),
        used: tick,
    });
    index
}

fn identity(meta: &std::fs::Metadata) -> Identity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (0, 0)
    }
}

fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as u64)
}

fn epoch(identity: Identity) -> u64 {
    identity.0.rotate_left(32) ^ identity.1
}

pub(crate) fn cursor(identity_epoch: u64, line: u64) -> String {
    format!("{identity_epoch:016x}.{line}")
}

fn parse_cursor(raw: &str) -> Result<(u64, u64), ReadError> {
    let (epoch, line) = raw.split_once('.').ok_or(ReadError::InvalidCursor)?;
    if epoch.len() != 16 || line.is_empty() || line.len() > 10 {
        return Err(ReadError::InvalidCursor);
    }
    let epoch = u64::from_str_radix(epoch, 16).map_err(|_| ReadError::InvalidCursor)?;
    let line = line.parse().map_err(|_| ReadError::InvalidCursor)?;
    Ok((epoch, line))
}

/// An open, current index for one transcript file.
struct Opened {
    file: File,
    index: Arc<Mutex<LineIndex>>,
    size: u64,
    mtime_ms: u64,
}

fn open_current(
    reader: &dyn TranscriptReader,
    root: &Path,
    path: &Path,
) -> Result<Opened, ReadError> {
    let mut file = fence::open(root, path).ok_or(ReadError::NotFound)?;
    let meta = file.metadata().map_err(|_| ReadError::Io)?;
    let (size, id) = (meta.len(), identity(&meta));
    let index = cached_index(path);
    {
        let mut guard = index.lock().unwrap_or_else(|poison| poison.into_inner());
        if guard.identity != id || size < guard.indexed_bytes {
            *guard = LineIndex {
                identity: id,
                ..LineIndex::default()
            };
        }
        if size > guard.indexed_bytes && !guard.extend(reader, &mut file, size) {
            *guard = LineIndex::default();
            return Err(ReadError::Io);
        }
    }
    Ok(Opened {
        file,
        index,
        size,
        mtime_ms: mtime_ms(&meta),
    })
}

fn read_line(file: &mut File, index: &LineIndex, line: u32) -> Option<Vec<u8>> {
    let start = *index.starts.get(line as usize)?;
    let end = index.line_end(line as usize);
    let length = usize::try_from(end - start).ok()?;
    if length > MAX_LINE_BYTES + 2 {
        return None;
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes).ok()?;
    let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    Some(trim(line).to_vec())
}

/// The cheap probe: index any appended lines and say how big the file is.
pub(crate) fn head(
    reader: &dyn TranscriptReader,
    root: &Path,
    path: &Path,
) -> Result<Head, ReadError> {
    let opened = open_current(reader, root, path)?;
    let index = opened.index.lock().unwrap_or_else(|p| p.into_inner());
    Ok(index.head(opened.size, opened.mtime_ms))
}

struct Lookup<'a> {
    file: &'a mut File,
    index: &'a LineIndex,
}

impl AnswerLookup for Lookup<'_> {
    fn answer(&mut self, key: &str) -> Option<SourceLine> {
        let line = *self.index.answers.get(key)?;
        let bytes = read_line(self.file, self.index, line)?;
        Some(SourceLine {
            line: u64::from(line),
            group_start: u64::from(line),
            bytes,
        })
    }
}

/// One page of visible lines strictly before `before` (the newest page when
/// absent), oldest first.
pub(crate) fn page(
    reader: &dyn TranscriptReader,
    root: &Path,
    path: &Path,
    before: Option<&str>,
    limit: usize,
) -> Result<Page, ReadError> {
    let before = before.map(parse_cursor).transpose()?;
    let mut opened = open_current(reader, root, path)?;
    let index = opened.index.lock().unwrap_or_else(|p| p.into_inner());
    let current = epoch(index.identity);
    let end = match before {
        None => index.visible.len(),
        Some((epoch, line)) => {
            if epoch != current || line > index.starts.len() as u64 {
                return Err(ReadError::CursorGone);
            }
            index
                .visible
                .partition_point(|visible| u64::from(*visible) < line)
        }
    };
    let limit = limit.clamp(1, MAX_PAGE_LIMIT);
    let mut start = end;
    let mut budget = 0_u64;
    while start > 0 && end - start < limit {
        let line = index.visible[start - 1] as usize;
        let size = index.line_end(line) - index.starts[line];
        if start < end && budget + size > MAX_PAGE_BYTES {
            break;
        }
        budget += size;
        start -= 1;
    }
    let mut lines = Vec::with_capacity(end - start);
    let mut skipped = 0;
    for at in start..end {
        match read_line(&mut opened.file, &index, index.visible[at]) {
            Some(bytes) => lines.push(SourceLine {
                line: u64::from(index.visible[at]),
                group_start: u64::from(index.group_start[at]),
                bytes,
            }),
            None => skipped += 1,
        }
    }
    let mut lookup = Lookup {
        file: &mut opened.file,
        index: &index,
    };
    let projection = reader.project(&lines, &mut lookup);
    Ok(Page {
        items: projection.items,
        has_more: start > 0,
        next_before: (start > 0).then(|| cursor(current, u64::from(index.visible[start]))),
        head: index.head(opened.size, opened.mtime_ms),
        skipped_lines: skipped + projection.malformed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, TimelineRole};
    use crate::agents::transcripts::Projection;
    use std::io::Write as _;

    /// A minimal reader: `{"v":"text"}` is visible, `{"g":"key","v":..}`
    /// groups, `{"answer":"key","v":..}` answers, `{}` is hidden, anything
    /// else is malformed.
    struct Lines;

    impl TranscriptReader for Lines {
        fn agent(&self) -> &'static str {
            "lines"
        }
        fn root(&self) -> Option<PathBuf> {
            None
        }
        fn valid_session_id(&self, _: &str) -> bool {
            true
        }
        fn locate(&self, _: &Path, _: &str, _: &super::super::LocateHints) -> Option<PathBuf> {
            None
        }
        fn classify(&self, line: &[u8]) -> Option<LineClass> {
            let value: serde_json::Value = serde_json::from_slice(line).ok()?;
            Some(LineClass {
                visible: value["v"].is_string() && value["answer"].is_null(),
                group: value["g"].as_str().map(str::to_owned),
                answers: value["answer"]
                    .as_str()
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
            })
        }
        fn project(&self, lines: &[SourceLine], lookup: &mut dyn AnswerLookup) -> Projection {
            let mut projection = Projection::default();
            for line in lines {
                let value: serde_json::Value = serde_json::from_slice(&line.bytes).unwrap();
                let mut text = value["v"].as_str().unwrap().to_owned();
                if let Some(key) = value["ask"].as_str() {
                    let answer = lookup.answer(key).map(|answer| {
                        let value: serde_json::Value =
                            serde_json::from_slice(&answer.bytes).unwrap();
                        value["v"].as_str().unwrap().to_owned()
                    });
                    text = format!("{text}={}", answer.unwrap_or_default());
                }
                projection.items.push(TimelineItem {
                    id: format!("{}", line.line),
                    message_id: format!("{}", line.group_start),
                    role: TimelineRole::User,
                    part: AgentPart::Text { text },
                    seq: line.line,
                    updated_ms: 0,
                    ordinal: 0,
                    attachments: None,
                    image_assets: None,
                });
            }
            projection
        }
    }

    fn texts(page: &Page) -> Vec<String> {
        page.items
            .iter()
            .map(|item| match &item.part {
                AgentPart::Text { text } => text.clone(),
                _ => unreachable!(),
            })
            .collect()
    }

    fn fixture(lines: &[&str]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir()
            .join(format!("transcript-index-{}", uuid::Uuid::new_v4()))
            .join("p");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.parent().unwrap().canonicalize().unwrap();
        let path = root.join("p/t.jsonl");
        std::fs::write(
            &path,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        (root, path)
    }

    fn append(path: &Path, raw: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(raw.as_bytes()).unwrap();
    }

    #[test]
    fn pages_run_newest_first_and_cursors_survive_growth() {
        let lines: Vec<String> = (0..7).map(|i| format!(r#"{{"v":"m{i}"}}"#)).collect();
        let mut all: Vec<&str> = lines.iter().map(String::as_str).collect();
        all.insert(3, "{}");
        all.insert(5, "not json");
        let (root, path) = fixture(&all);

        let first = page(&Lines, &root, &path, None, 3).unwrap();
        assert_eq!(texts(&first), ["m4", "m5", "m6"]);
        assert!(first.has_more);
        assert_eq!(first.head.lines, 9);
        assert_eq!(first.head.entries, 7);
        assert_eq!(first.head.malformed_lines, 1);

        // The file grows, including a half-written last line: the old cursor
        // still means the same place, and the partial line is not indexed.
        append(&path, "{\"v\":\"m7\"}\n{\"v\":\"m8");
        let cursor = first.next_before.clone().unwrap();
        let second = page(&Lines, &root, &path, Some(&cursor), 3).unwrap();
        assert_eq!(texts(&second), ["m1", "m2", "m3"]);
        assert_eq!(second.head.lines, 10);
        let third = page(&Lines, &root, &path, second.next_before.as_deref(), 3).unwrap();
        assert_eq!(texts(&third), ["m0"]);
        assert!(!third.has_more);
        assert_eq!(third.next_before, None);

        // Completing the line makes it visible on the next newest page.
        append(&path, "\"}\n");
        let newest = page(&Lines, &root, &path, None, 2).unwrap();
        assert_eq!(texts(&newest), ["m7", "m8"]);
        assert_eq!(head(&Lines, &root, &path).unwrap().entries, 9);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_replaced_or_truncated_file_invalidates_old_cursors() {
        let (root, path) = fixture(&[r#"{"v":"a"}"#, r#"{"v":"b"}"#, r#"{"v":"c"}"#]);
        let first = page(&Lines, &root, &path, None, 1).unwrap();
        let cursor = first.next_before.unwrap();
        // Replaced: a new inode.
        let replacement = path.with_extension("new");
        std::fs::write(&replacement, "{\"v\":\"x\"}\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(
            page(&Lines, &root, &path, Some(&cursor), 1).unwrap_err(),
            ReadError::CursorGone
        );
        assert_eq!(texts(&page(&Lines, &root, &path, None, 5).unwrap()), ["x"]);
        // Truncated in place: reindexed from scratch.
        std::fs::write(&path, "").unwrap();
        assert_eq!(head(&Lines, &root, &path).unwrap().lines, 0);
        assert_eq!(
            page(&Lines, &root, &path, Some("zz"), 1).unwrap_err(),
            ReadError::InvalidCursor
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn groups_share_their_first_line_and_answers_resolve_beyond_the_page() {
        let (root, path) = fixture(&[
            r#"{"v":"q"}"#,
            r#"{"g":"A","v":"a1","ask":"k"}"#,
            r#"{"answer":"k","v":"done"}"#,
            r#"{"g":"A","v":"a2"}"#,
            r#"{"g":"B","v":"b1"}"#,
        ]);
        let newest = page(&Lines, &root, &path, None, 2).unwrap();
        let groups: Vec<&str> = newest.items.iter().map(|i| i.message_id.as_str()).collect();
        assert_eq!(groups, ["1", "4"]);
        let older = page(&Lines, &root, &path, newest.next_before.as_deref(), 5).unwrap();
        assert_eq!(texts(&older), ["q", "a1=done"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_path_outside_the_root_is_not_found() {
        let (root, path) = fixture(&[r#"{"v":"a"}"#]);
        let (other_root, _) = fixture(&[]);
        assert_eq!(
            head(&Lines, &other_root, &path).unwrap_err(),
            ReadError::NotFound
        );
        assert!(head(&Lines, &root, &path).is_ok());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other_root);
    }
}
