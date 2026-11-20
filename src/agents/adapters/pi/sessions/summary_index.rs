//! Keep small projections in memory. Large native histories spill to a private
//! SQLite temporary database, which SQLite removes on close (also on error).

use std::io;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::{
    session_info, summarize, AgentSessionInfo, AgentSessionStatus, BranchSummary, SessionFile,
    MAX_SESSION_BYTES, MAX_SUMMARY_ENTRIES,
};

#[derive(Default)]
pub(super) struct SummaryIndex {
    memory: Vec<Value>,
    bytes: usize,
    disk: Option<DiskIndex>,
}

impl SummaryIndex {
    pub(super) fn push(&mut self, entry: Value) -> io::Result<()> {
        if self.disk.is_none() {
            self.bytes = self
                .bytes
                .saturating_add(super::super::mapper::value_bytes(&entry));
            if self.bytes <= MAX_SESSION_BYTES && self.memory.len() < MAX_SUMMARY_ENTRIES {
                self.memory.push(entry);
                return Ok(());
            }
            let mut disk = DiskIndex::new().map_err(io::Error::other)?;
            for old in std::mem::take(&mut self.memory) {
                disk.insert(old).map_err(io::Error::other)?;
            }
            self.disk = Some(disk);
        }
        self.disk
            .as_mut()
            .unwrap()
            .insert(entry)
            .map_err(io::Error::other)
    }

    pub(super) fn finish(
        self,
        file: &SessionFile,
        status: AgentSessionStatus,
    ) -> io::Result<AgentSessionInfo> {
        match self.disk {
            None => Ok(session_info(file, &self.memory, status)),
            Some(disk) => disk.summarize(file, status).map_err(io::Error::other),
        }
    }
}

struct DiskIndex {
    db: Connection,
    header: Option<Value>,
    leaf: Option<i64>,
}

impl DiskIndex {
    fn new() -> rusqlite::Result<Self> {
        let db = Connection::open("")?;
        db.execute_batch("PRAGMA cache_size=-1024; PRAGMA temp_store=FILE;
            PRAGMA journal_mode=OFF; BEGIN;
            CREATE TABLE entries(n INTEGER PRIMARY KEY,id TEXT UNIQUE,parent TEXT,payload BLOB NOT NULL,depth INTEGER UNIQUE);")?;
        Ok(Self {
            db,
            header: None,
            leaf: None,
        })
    }

    fn insert(&mut self, entry: Value) -> rusqlite::Result<()> {
        if entry["type"] == "session" {
            if self.header.is_none() {
                self.header = Some(entry);
            }
            return Ok(());
        }
        // Replacement matches the in-memory lookup's last-write-wins rule for
        // duplicate native ids. The explicit leaf also supports an id-less row.
        self.db
            .prepare_cached("INSERT OR REPLACE INTO entries(id,parent,payload) VALUES (?,?,?)")?
            .execute(params![
                entry["id"].as_str(),
                entry["parentId"].as_str(),
                serde_json::to_vec(&entry).expect("JSON value serializes")
            ])?;
        self.leaf = Some(self.db.last_insert_rowid());
        Ok(())
    }

    fn summarize(
        self,
        file: &SessionFile,
        status: AgentSessionStatus,
    ) -> rusqlite::Result<AgentSessionInfo> {
        let mut leaf = self.leaf;
        let mut mark = self
            .db
            .prepare("UPDATE entries SET depth=? WHERE n=? AND depth IS NULL RETURNING parent")?;
        let mut lookup = self.db.prepare("SELECT n FROM entries WHERE id=?")?;
        let mut depth = 0i64;
        while let Some(n) = leaf {
            let parent: Option<String> = mark
                .query_row(params![depth, n], |r| r.get(0))
                .optional()?
                .flatten();
            let Some(parent) = parent else { break };
            leaf = lookup.query_row([parent], |r| r.get(0)).optional()?;
            depth += 1;
        }
        let mut summary = BranchSummary::default();
        let mut ordered = self
            .db
            .prepare("SELECT payload FROM entries WHERE depth IS NOT NULL ORDER BY depth DESC")?;
        let mut rows = ordered.query([])?;
        while let Some(row) = rows.next()? {
            let payload = row.get_ref(0)?.as_blob()?;
            let entry = serde_json::from_slice(payload).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    Box::new(e),
                )
            })?;
            summary.observe(&entry);
        }
        Ok(summarize(file, self.header.as_ref(), status, summary))
    }
}
