//! Full-text search over the content of every agent session on this
//! machine: `GET /api/search`.
//!
//! The index is a local SQLite FTS5 table with the trigram tokenizer
//! ([`index`]), so any substring of three or more characters matches --
//! which is what Chinese and Japanese text needs -- and shorter terms are a
//! substring scan over the same rows. It holds user prompts, assistant
//! replies and tool-call headlines ([`text::rows`]), served through the same
//! hiding rules as the transcript (`tasks::transcript`), and never leaves
//! the machine.
//!
//! [`indexer`] builds it in the background from each agent's own store
//! without waking a session ([`sources`]), then keeps it current from the
//! agent domain events and a periodic catch-up keyed by each session's
//! watermark, which survives restarts.
//!
//! Linked computers: a search is answered from this machine's index alone.
//! [`SearchService::search`] is the seam a federated search would call on
//! each peer (through `fleet::peer_client`) before merging pages by score.

pub(crate) mod index;
pub(crate) mod indexer;
pub(crate) mod routes;
pub(crate) mod sources;
pub(crate) mod text;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::platform::store::db_key::DbKey;
use crate::platform::store::sqlite::SqliteStore;
use index::{Filters, Hit};
use text::Query;

pub(crate) const CAPABILITY: &str = "session_search_v1";

/// How many hits one session contributes to a page of results.
pub(crate) const HITS_PER_SESSION: usize = 3;
/// The rows ranked per query, before grouping.
const CANDIDATES: usize = 1000;

/// How far the background build has come: sessions whose index is current
/// out of the sessions known.
#[derive(Default)]
pub(crate) struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub(crate) struct Indexing {
    pub done: usize,
    pub total: usize,
}

/// A session something happened in, waiting to be read again.
#[derive(Debug, Clone)]
pub(crate) struct Dirty {
    pub agent_id: Option<String>,
    pub first: std::time::Instant,
    pub last: std::time::Instant,
    /// Rows the live stream removed (a live-only agent's).
    pub removed: Vec<String>,
    /// The session as its last `agent.session.updated` described it.
    pub meta: Option<index::SessionMeta>,
    pub deleted: bool,
}

pub(crate) struct SearchService {
    /// The indexer's connection, on a low-priority thread of its own.
    writer: Option<Arc<SqliteStore>>,
    /// Queries read beside the writer (WAL), never queued behind a build.
    reader: Option<Arc<SqliteStore>>,
    pub(crate) progress: Progress,
    pub(crate) dirty: Mutex<HashMap<String, Dirty>>,
    pub(crate) wake: tokio::sync::Notify,
}

/// One search, validated.
#[derive(Debug, Clone)]
pub(crate) struct SearchRequest {
    pub query: Query,
    pub limit: usize,
    pub offset: usize,
    pub filters: Filters,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct SearchItem {
    pub asid: String,
    pub agent_id: String,
    pub title: String,
    pub directory: Option<String>,
    /// The session's parent, for a subagent's session.
    pub parent_asid: Option<String>,
    pub item_id: String,
    pub message_id: String,
    pub role: String,
    /// `text` (a prompt or reply) or `tool` (a tool call's headline).
    pub kind: String,
    pub time_ms: u64,
    pub snippet: String,
    /// `[start, end)` in UTF-16 code units of `snippet`.
    pub highlights: Vec<[usize; 2]>,
    /// The session's matching rows among those ranked, of which at most
    /// [`HITS_PER_SESSION`] are listed.
    pub session_hits: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct SearchPage {
    pub items: Vec<SearchItem>,
    pub next_cursor: Option<String>,
    pub indexing: Indexing,
}

const SEARCH_FILE: &str = "search.sqlite3";

impl SearchService {
    /// The index under the store directory, keyed like the store. A file
    /// that does not open (another key, a newer schema, damage) is derived
    /// data: it is deleted and built again.
    pub(crate) fn open(state: &Path, key: Option<DbKey>) -> SearchService {
        let path = crate::platform::store::store_db_path(state).with_file_name(SEARCH_FILE);
        let open = || -> anyhow::Result<(Arc<SqliteStore>, Arc<SqliteStore>)> {
            let writer = SqliteStore::open(&path, key.clone(), &[index::MIGRATION])?;
            let reader = SqliteStore::open(&path, key.clone(), &[index::MIGRATION])?;
            Ok((writer, reader))
        };
        let opened = open().or_else(|error| {
            tracing::warn!("search index {} is rebuilt: {error}", path.display());
            for suffix in ["", "-wal", "-shm"] {
                let mut file = path.clone().into_os_string();
                file.push(suffix);
                let _ = std::fs::remove_file(file);
            }
            open()
        });
        match opened {
            Ok((writer, reader)) => SearchService::with(Some(writer), Some(reader)),
            Err(error) => {
                tracing::warn!("session search is unavailable: {error}");
                SearchService::with(None, None)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> SearchService {
        SearchService::with(None, None)
    }

    fn with(writer: Option<Arc<SqliteStore>>, reader: Option<Arc<SqliteStore>>) -> SearchService {
        SearchService {
            writer,
            reader,
            progress: Progress::default(),
            dirty: Mutex::default(),
            wake: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn writer(&self) -> Option<&Arc<SqliteStore>> {
        self.writer.as_ref()
    }

    pub(crate) fn indexing(&self) -> Indexing {
        let total = self.progress.total.load(Ordering::Relaxed);
        Indexing {
            done: self.progress.done.load(Ordering::Relaxed).min(total),
            total,
        }
    }

    /// A session to read again soon: something happened in it.
    pub(crate) fn touch(&self, asid: &str, update: impl FnOnce(&mut Dirty)) {
        let now = std::time::Instant::now();
        let mut dirty = self.dirty.lock().unwrap_or_else(|p| p.into_inner());
        let entry = dirty.entry(asid.to_owned()).or_insert_with(|| Dirty {
            agent_id: None,
            first: now,
            last: now,
            removed: Vec::new(),
            meta: None,
            deleted: false,
        });
        entry.last = now;
        update(entry);
    }

    /// One page of hits, ranked and grouped by session. `None` while the
    /// index is unavailable.
    pub(crate) async fn search(
        &self,
        request: SearchRequest,
    ) -> anyhow::Result<Option<SearchPage>> {
        let Some(reader) = &self.reader else {
            return Ok(None);
        };
        let (query, filters) = (request.query.clone(), request.filters.clone());
        let hits = reader
            .call(move |c| Ok(index::search(c, &query, &filters, CANDIDATES)?))
            .await?;
        let ranked = rank(hits, crate::now_unix_ms() as u64, HITS_PER_SESSION);
        let end = (request.offset + request.limit).min(ranked.len());
        let next_cursor = (end < ranked.len()).then(|| end.to_string());
        let items = ranked
            .into_iter()
            .skip(request.offset)
            .take(request.limit)
            .map(|(hit, session_hits)| {
                let (snippet, highlights) = text::snippet(&hit.text, &request.query);
                SearchItem {
                    asid: hit.meta.asid,
                    agent_id: hit.meta.agent_id,
                    title: hit.meta.title,
                    directory: hit.meta.directory,
                    parent_asid: hit.meta.parent_asid,
                    item_id: hit.item_id,
                    message_id: hit.message_id,
                    role: hit.role,
                    kind: hit.kind,
                    time_ms: hit.time_ms,
                    snippet,
                    highlights,
                    session_hits,
                }
            })
            .collect();
        Ok(Some(SearchPage {
            items,
            next_cursor,
            indexing: self.indexing(),
        }))
    }
}

/// The score a hit ranks by: text relevance (bm25), weighted towards
/// recent rows (half the weight after 30 days, never below 30%), and a
/// little towards what the user wrote over tool headlines.
fn score(hit: &Hit, now_ms: u64) -> f64 {
    let relevance = 1.0 + hit.bm25.map_or(0.0, |b| (-b).max(0.0));
    let age_days = now_ms.saturating_sub(hit.time_ms) as f64 / 86_400_000.0;
    let recency = 0.3 + 0.7 * 0.5f64.powf(age_days / 30.0);
    let kind = match (hit.role.as_str(), hit.kind.as_str()) {
        ("user", _) => 1.2,
        (_, "tool") => 0.8,
        _ => 1.0,
    };
    relevance * recency * kind
}

/// Hits by score, at most `per_session` from each session, the sessions in
/// the order of their best hit and each session's hits together, with the
/// number of the session's hits among those ranked.
fn rank(hits: Vec<Hit>, now_ms: u64, per_session: usize) -> Vec<(Hit, usize)> {
    let mut scored: Vec<(f64, Hit)> = hits.into_iter().map(|h| (score(&h, now_ms), h)).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), (usize, Vec<Hit>)> = HashMap::new();
    for (_, hit) in scored {
        let key = (hit.meta.agent_id.clone(), hit.meta.asid.clone());
        let group = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (0, Vec::new())
        });
        group.0 += 1;
        if group.1.len() < per_session {
            group.1.push(hit);
        }
    }
    order
        .into_iter()
        .flat_map(|key| {
            let (count, hits) = groups.remove(&key).unwrap_or_default();
            hits.into_iter().map(move |hit| (hit, count))
        })
        .collect()
}

#[cfg(test)]
mod tests;
