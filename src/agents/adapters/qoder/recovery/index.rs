//! Temporary recovery evidence contains branch IDs and file locations only.
use std::collections::HashMap;

use rusqlite::{params, Connection};
use tokio_util::sync::CancellationToken;

use super::{unavailable, AgentError, Location};

const MEMORY_LOCATIONS: usize = 16_384;
type Span = (u64, usize, bool);

pub(super) enum Index {
    Memory(HashMap<String, Location>),
    Disk(Scratch),
}

pub(super) struct Scratch {
    db: Connection,
    // The connection must close before the private directory is removed.
    _dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Result<Self, AgentError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("muqun-qoder-index-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let dir = builder.tempdir().map_err(|_| unavailable())?;
        let db = Connection::open(dir.path().join("index.sqlite")).map_err(|_| unavailable())?;
        // Private and disposable. A failed inspection drops the entire database.
        db.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA mmap_size=0;
             PRAGMA cache_size=-2048;
             CREATE TABLE node (id TEXT PRIMARY KEY, parent TEXT, offset INTEGER NOT NULL,
               size INTEGER NOT NULL, human INTEGER NOT NULL, rank INTEGER) WITHOUT ROWID;
             CREATE UNIQUE INDEX branch ON node(rank) WHERE rank IS NOT NULL;
             BEGIN",
        )
        .map_err(|_| unavailable())?;
        Ok(Self { db, _dir: dir })
    }

    fn insert(&self, id: &str, node: Location) -> Result<(), AgentError> {
        // An empty ID cannot be a parent, so it retains the invalid-parent
        // marker for branches the native leaf might never select.
        let parent = node.parent.unwrap_or_else(|()| Some(String::new()));
        self.db
            .prepare_cached("INSERT INTO node(id,parent,offset,size,human) VALUES(?,?,?,?,?)")
            .and_then(|mut stmt| {
                stmt.execute(params![
                    id,
                    parent,
                    i64::try_from(node.offset).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    node.length as i64,
                    node.human
                ])
            })
            .map(|_| ())
            .map_err(|_| unavailable())
    }
    fn visit(&self, id: &str, rank: usize) -> Result<Location, AgentError> {
        let mut stmt = self.db.prepare_cached(
            "UPDATE node SET rank=? WHERE id=? AND rank IS NULL RETURNING offset,size,human,parent",
        ).map_err(|_| unavailable())?;
        stmt.query_row(params![rank as i64, id], |row| {
            let parent: Option<String> = row.get(3)?;
            Ok(Location {
                offset: row.get::<_, i64>(0)? as u64,
                length: row.get::<_, i64>(1)? as usize,
                human: row.get(2)?,
                parent: if parent.as_deref() == Some("") {
                    Err(())
                } else {
                    Ok(parent)
                },
            })
        })
        .map_err(|_| unavailable())
    }
}

impl Index {
    pub fn new() -> Self {
        Self::Memory(HashMap::new())
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Memory(nodes) if nodes.is_empty())
    }

    pub fn insert(
        &mut self,
        id: &str,
        location: Location,
        stop: &CancellationToken,
    ) -> Result<(), AgentError> {
        if let Self::Memory(nodes) = self {
            if nodes.len() < MEMORY_LOCATIONS {
                return if nodes.insert(id.to_owned(), location).is_none() {
                    Ok(())
                } else {
                    Err(unavailable())
                };
            }
            let scratch = Scratch::new()?;
            for (id, node) in nodes.drain() {
                if stop.is_cancelled() {
                    return Err(unavailable());
                }
                scratch.insert(&id, node)?;
            }
            *self = Self::Disk(scratch);
        }
        match self {
            Self::Disk(scratch) => scratch.insert(id, location),
            Self::Memory(_) => unreachable!("index spilled above"),
        }
    }

    pub fn trace(mut self, mut id: String, stop: &CancellationToken) -> Result<Branch, AgentError> {
        let mut spans = Vec::new();
        let mut len = 0;
        let mut last_human = None;
        loop {
            if stop.is_cancelled() {
                return Err(unavailable());
            }
            let node = match &mut self {
                Self::Memory(nodes) => nodes.remove(&id).ok_or_else(unavailable)?,
                Self::Disk(scratch) => scratch.visit(&id, len)?,
            };
            if matches!(self, Self::Memory(_)) {
                spans.push((node.offset, node.length, node.human));
            }
            if node.human && last_human.is_none() {
                last_human = Some(len);
            }
            len += 1;
            match node.parent.map_err(|_| unavailable())? {
                None => break,
                Some(parent) => id = parent,
            }
        }
        let source = match self {
            Self::Memory(_) => Source::Memory(spans),
            Self::Disk(scratch) => Source::Disk(scratch),
        };
        Ok(Branch {
            source,
            len,
            current_turn: last_human.map_or(0, |rank| len - 1 - rank),
        })
    }
}

enum Source {
    Memory(Vec<Span>),
    Disk(Scratch),
}

pub(super) struct Branch {
    source: Source,
    pub len: usize,
    pub current_turn: usize,
}

impl Branch {
    pub fn at(&self, index: usize) -> Result<Span, AgentError> {
        let rank = self.len - 1 - index;
        match &self.source {
            Source::Memory(spans) => Ok(spans[rank]),
            Source::Disk(scratch) => scratch
                .db
                .prepare_cached("SELECT offset,size,human FROM node WHERE rank=?")
                .and_then(|mut stmt| {
                    stmt.query_row([rank as i64], |row| {
                        Ok((
                            row.get::<_, i64>(0)? as u64,
                            row.get::<_, i64>(1)? as usize,
                            row.get(2)?,
                        ))
                    })
                })
                .map_err(|_| unavailable()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_spill_is_removed_after_success_and_cancellation() {
        for cancelled in [false, true] {
            let stop = CancellationToken::new();
            let mut index = Index::new();
            for id in 0..=MEMORY_LOCATIONS {
                index
                    .insert(
                        &id.to_string(),
                        Location {
                            offset: id as u64,
                            length: 1,
                            human: true,
                            parent: Ok(None),
                        },
                        &stop,
                    )
                    .unwrap();
            }
            let Index::Disk(scratch) = &index else {
                panic!("large index did not spill")
            };
            let path = scratch._dir.path().to_owned();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
            }
            assert!(path.join("index.sqlite").is_file());
            if cancelled {
                stop.cancel();
            }
            let result = index.trace("1".into(), &stop);
            if cancelled {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().at(0).unwrap(), (1, 1, true));
            }
            assert!(!path.exists());
        }
    }
}
