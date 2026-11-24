//! Keep Pi's branch and row coordinates in memory. Materialize payloads only for
//! the rows requested by the caller; a tool result can be far from its call.
use super::{mapper, records, sessions};
use crate::agents::domain::{AgentPart, ModelRef, TimelineItem};
use crate::agents::ports::AgentError;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use tokio::sync::OnceCell;

const RECORD_LIMIT: usize = super::rpc::MAX_LINE_BYTES;
const CACHED_SESSIONS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Span {
    offset: u64,
    len: u64,
    large: bool,
}

impl Span {
    fn value(self, f: &mut File) -> io::Result<Value> {
        f.seek(SeekFrom::Start(self.offset))?;
        let read = BufReader::new(f.take(self.len));
        if self.large {
            records::project_large(read)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Pi entry changed while reading history",
                )
            })
        } else {
            serde_json::from_reader(read).map_err(io::Error::other)
        }
    }
}

struct Row {
    id: String,
    message: String,
    ordinal: u64,
    rank: u8,
    span: Span,
    block: usize,
    seq: u64,
    tool: Option<String>,
    result: Option<Span>,
    previous: Option<ModelRef>,
}
impl Row {
    fn key(&self) -> (&str, u64, u8, &str) {
        (&self.message, self.ordinal, self.rank, &self.id)
    }
}

struct Entry {
    id: Option<String>,
    parent: Option<String>,
    rows: Vec<Row>,
    result: Option<(String, Span)>,
    model: Option<ModelRef>,
    usage: Option<Value>,
}

impl Entry {
    fn parse(value: &Value, span: Span) -> Self {
        let mut entry = Self {
            id: value["id"].as_str().map(str::to_owned),
            parent: value["parentId"].as_str().map(str::to_owned),
            rows: Vec::new(),
            result: None,
            model: None,
            usage: None,
        };
        match value.pointer("/message/role").and_then(Value::as_str) {
            Some("toolResult") if value["type"] == "message" => {
                entry.result = value
                    .pointer("/message/toolCallId")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_owned(), span));
            }
            Some("assistant") => entry.usage = value.pointer("/message/usage").cloned(),
            _ => {}
        }
        for (block, row) in mapper::timeline_from_entries(std::slice::from_ref(value))
            .into_iter()
            .enumerate()
        {
            let rank = row.sort_key().2;
            let tool = match &row.part {
                AgentPart::Tool(t) => Some(t.id.clone()),
                AgentPart::ModelSwitched { model, .. } => {
                    entry.model = Some(model.clone());
                    None
                }
                _ => None,
            };
            entry.rows.push(Row {
                id: row.id,
                message: row.message_id,
                ordinal: row.ordinal,
                rank,
                span,
                block,
                seq: 0,
                tool,
                result: None,
                previous: None,
            });
        }
        entry
    }
}

#[derive(Default)]
pub(super) struct Index {
    rows: Vec<Row>,
    stamp: Option<Stamp>,
    positions: HashMap<String, usize>,
    pub(super) usage: mapper::UsageTotals,
}

impl Index {
    fn build(f: File, stamp: Stamp) -> io::Result<Self> {
        let mut entries = Vec::new();
        let mut by_id = HashMap::new();
        records::scan(f, RECORD_LIMIT, |offset, len, record| {
            let large = matches!(record, records::Record::Large(_));
            let value = match record {
                records::Record::Raw(line) => match serde_json::from_slice(line) {
                    Ok(v) => v,
                    Err(_) => return true,
                },
                records::Record::Large(v) => v,
                records::Record::Malformed => return true,
            };
            if value["type"] == "session" {
                return true;
            }
            let entry = Entry::parse(&value, Span { offset, len, large });
            if let Some(id) = &entry.id {
                by_id.insert(id.clone(), entries.len());
            }
            entries.push(Some(entry));
            true
        })?;
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = entries.len().checked_sub(1);
        while let Some(at) = cur {
            let Some(entry) = entries[at].take() else {
                break;
            };
            if let Some(id) = &entry.id {
                if !seen.insert(id.clone()) {
                    break;
                }
            }
            cur = entry.parent.as_ref().and_then(|id| by_id.get(id)).copied();
            chain.push(entry);
        }
        let mut index = Self {
            stamp: Some(stamp),
            ..Default::default()
        };
        let mut tools = HashMap::<String, usize>::new();
        let mut model = None;
        for entry in chain.into_iter().rev() {
            if let Some(usage) = entry.usage {
                index.usage.add(&usage);
            }
            if let Some((id, span)) = entry.result {
                if let Some(&n) = tools.get(&id) {
                    index.rows[n].result = Some(span);
                }
            }
            for mut row in entry.rows {
                if let Some(tool) = row.tool.take() {
                    tools.insert(tool, index.rows.len());
                }
                if let Some(next) = &entry.model {
                    row.previous = model.replace(next.clone());
                }
                row.seq = index.rows.len() as u64;
                index.rows.push(row);
            }
        }
        index.rows.sort_by(|a, b| a.key().cmp(&b.key()));
        index.positions = index
            .rows
            .iter()
            .enumerate()
            .map(|(n, r)| (r.id.clone(), n))
            .collect();
        Ok(index)
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
    pub(super) fn contains(&self, id: &str) -> bool {
        self.positions.contains_key(id)
    }

    pub(super) fn page(
        &self,
        mut file: Option<File>,
        live: Vec<TimelineItem>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TimelineItem>, AgentError> {
        let verify = |file: &Option<File>| {
            if let (Some(f), Some(stamp)) = (file, &self.stamp) {
                if Stamp::from(&f.metadata().map_err(error)?) != *stamp {
                    return Err(AgentError::HistoryCursorLost);
                }
            }
            Ok(())
        };
        verify(&file)?;
        enum Source<'a> {
            Disk(&'a Row),
            Live(&'a TimelineItem),
        }
        impl Source<'_> {
            fn key(&self) -> (&str, u64, u8, &str) {
                match self {
                    Self::Disk(r) => r.key(),
                    Self::Live(r) => r.sort_key(),
                }
            }
        }
        let selected: Vec<_> = if live.is_empty() {
            let end = match before {
                Some(id) => *self
                    .positions
                    .get(id)
                    .ok_or(AgentError::HistoryCursorLost)?,
                None => self.rows.len(),
            };
            self.rows[end.saturating_sub(limit)..end]
                .iter()
                .map(Source::Disk)
                .collect()
        } else {
            let mut order: Vec<_> = self
                .rows
                .iter()
                .map(Source::Disk)
                .chain(live.iter().map(Source::Live))
                .collect();
            order.sort_by(|a, b| a.key().cmp(&b.key()));
            let end = match before {
                Some(id) => order
                    .iter()
                    .position(|r| r.key().3 == id)
                    .ok_or(AgentError::HistoryCursorLost)?,
                None => order.len(),
            };
            order.drain(end.saturating_sub(limit)..end).collect()
        };
        let mut last_message: Option<(Span, Vec<TimelineItem>)> = None;
        let mut out = Vec::with_capacity(selected.len());
        for source in selected {
            let row = match source {
                Source::Live(item) => item.clone(),
                Source::Disk(row) => {
                    let f = file.as_mut().ok_or(AgentError::HistoryCursorLost)?;
                    let entry = match &mut last_message {
                        Some((span, items)) if *span == row.span => items,
                        _ => {
                            let value = row.span.value(f).map_err(error)?;
                            &mut last_message
                                .insert((
                                    row.span,
                                    mapper::timeline_from_entries(std::slice::from_ref(&value)),
                                ))
                                .1
                        }
                    };
                    let mut item = entry
                        .get(row.block)
                        .filter(|item| item.id == row.id)
                        .cloned()
                        .ok_or(AgentError::HistoryCursorLost)?;
                    item.seq = row.seq;
                    if let AgentPart::ModelSwitched { previous, .. } = &mut item.part {
                        previous.clone_from(&row.previous);
                    }
                    if let Some(span) = row.result {
                        let value = span.value(f).map_err(error)?;
                        mapper::apply_history_outcome(&mut item, &value);
                    }
                    item
                }
            };
            out.push(row);
        }
        verify(&file)?;
        Ok(out)
    }
}

/// Observe only committed JSONL records. Pi may finish the last line after its RPC event.
pub(super) fn appended_messages(mut file: File, start: u64) -> io::Result<(u64, Vec<Value>)> {
    let len = file.metadata()?.len();
    if len <= start {
        return Ok((start, Vec::new()));
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    let mut end = start;
    let mut entries = Vec::new();
    records::scan_from(file, start, RECORD_LIMIT, |offset, bytes, record| {
        let next = offset + bytes;
        if next > len || (next == len && last[0] != b'\n') {
            return false;
        }
        end = next;
        let value = match record {
            records::Record::Raw(line) => serde_json::from_slice(line).ok(),
            records::Record::Large(v) => Some(v),
            records::Record::Malformed => None,
        };
        if let Some(v) = value.filter(|v| v["type"] == "message") {
            entries.push(v);
        }
        true
    })?;
    Ok((end, entries))
}

fn error(e: io::Error) -> AgentError {
    AgentError::Protocol(format!("pi history: {e}"))
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
impl From<&std::fs::Metadata> for Stamp {
    fn from(m: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            size: m.len(),
            modified: m.modified().ok(),
            #[cfg(unix)]
            identity: (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()),
        }
    }
}

type Slot = Arc<OnceCell<Arc<Index>>>;
#[derive(Default)]
pub(super) struct Histories(Mutex<HashMap<PathBuf, (Stamp, Slot)>>);

impl Histories {
    pub(super) async fn open(&self, path: &Path) -> Result<(File, Arc<Index>, u64), AgentError> {
        let file = sessions::open_regular(path)
            .await
            .map_err(error)?
            .into_std()
            .await;
        let stamp = Stamp::from(&file.metadata().map_err(error)?);
        let size = stamp.size;
        let slot = {
            let mut map = self.0.lock().map_err(|_| AgentError::HistoryCursorLost)?;
            if let Some((_, slot)) = map.get(path).filter(|(s, _)| s == &stamp) {
                slot.clone()
            } else {
                if map.len() >= CACHED_SESSIONS && !map.contains_key(path) {
                    if let Some(key) = map.keys().next().cloned() {
                        map.remove(&key);
                    }
                }
                let slot = Slot::default();
                map.insert(path.to_owned(), (stamp.clone(), slot.clone()));
                slot
            }
        };
        let index = slot
            .get_or_try_init(|| async {
                let source = file.try_clone().map_err(error)?;
                tokio::task::spawn_blocking(move || Index::build(source, stamp).map(Arc::new))
                    .await
                    .map_err(|e| AgentError::Protocol(e.to_string()))?
                    .map_err(error)
            })
            .await?
            .clone();
        if index.stamp.as_ref() != Some(&Stamp::from(&file.metadata().map_err(error)?)) {
            return Err(AgentError::HistoryCursorLost);
        }
        Ok((file, index, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn range_reads_detect_in_place_rewrites_and_follow_replacements() {
        let dir = std::env::temp_dir().join(format!("pi-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let write = |path: &Path, text: &str| {
            let entry = serde_json::json!({"type":"message","id":"shared-id","message":{"role":"user","content":text}});
            std::fs::write(path, format!("{entry}\n")).unwrap();
        };
        write(&path, "a");
        let cache = Histories::default();
        let (file, stale, _) = cache.open(&path).await.unwrap();
        write(&path, "different content under same ID");
        assert!(matches!(
            stale.page(Some(file), vec![], None, 1),
            Err(AgentError::HistoryCursorLost)
        ));
        let (_, current, _) = cache.open(&path).await.unwrap();
        let replacement = dir.join("replacement");
        write(&replacement, "new inode");
        std::fs::rename(&replacement, &path).unwrap();
        let (file, index, _) = cache.open(&path).await.unwrap();
        assert!(!Arc::ptr_eq(&current, &index));
        let items = index.page(Some(file), vec![], None, 1).unwrap();
        assert!(matches!(&items[0].part,AgentPart::Text{text} if text=="new inode"));
        let (_, unchanged, _) = cache.open(&path).await.unwrap();
        assert!(Arc::ptr_eq(&index, &unchanged));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
