//! Current storage adapter: captures come from the separate live fold policy;
//! pinned pages have their own byte/count/TTL budget. No I/O while locked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use super::history::{
    Capture, HistoryError, HistoryFuture, HistoryRepository, HistoryScope, HistorySnapshot,
    SNAPSHOT_TTL,
};
use super::scrollback::{ScrollbackStore, MAX_PANE_BYTES};

const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOTS: usize = 32;
const MAX_SESSION_SNAPSHOTS: usize = 8;
const MAX_PANE_SNAPSHOTS: usize = 2;

pub(crate) struct MemoryHistoryRepository {
    captures: Arc<Mutex<ScrollbackStore>>,
    snapshots: Mutex<HashMap<Uuid, Arc<HistorySnapshot>>>,
}

impl MemoryHistoryRepository {
    pub(crate) fn new(captures: Arc<Mutex<ScrollbackStore>>) -> Self {
        Self {
            captures,
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    fn prune(
        &self,
        snapshots: &mut HashMap<Uuid, Arc<HistorySnapshot>>,
    ) -> Result<(), HistoryError> {
        let captures = self
            .captures
            .lock()
            .map_err(|_| HistoryError::Unavailable)?;
        snapshots.retain(|_, snap| {
            snap.created.elapsed() < SNAPSHOT_TTL
                && captures.capture_epoch(&snap.scope) == Some(snap.epoch)
        });
        Ok(())
    }
}

impl HistoryRepository for MemoryHistoryRepository {
    fn capture<'a>(
        &'a self,
        scope: &'a HistoryScope,
        include_rows: bool,
    ) -> HistoryFuture<'a, Option<Capture>> {
        Box::pin(async move {
            self.captures
                .lock()
                .map_err(|_| HistoryError::Unavailable)?
                .captured_history(scope, include_rows)
        })
    }

    fn find<'a>(
        &'a self,
        scope: &'a HistoryScope,
    ) -> HistoryFuture<'a, Option<Arc<HistorySnapshot>>> {
        Box::pin(async move {
            let mut snapshots = self
                .snapshots
                .lock()
                .map_err(|_| HistoryError::Unavailable)?;
            self.prune(&mut snapshots)?;
            Ok(snapshots
                .values()
                .find(|snap| snap.scope == *scope)
                .cloned())
        })
    }

    fn load(&self, id: Uuid) -> HistoryFuture<'_, Option<Arc<HistorySnapshot>>> {
        Box::pin(async move {
            let mut snapshots = self
                .snapshots
                .lock()
                .map_err(|_| HistoryError::Unavailable)?;
            self.prune(&mut snapshots)?;
            Ok(snapshots.get(&id).cloned())
        })
    }

    fn pin(&self, snapshot: HistorySnapshot) -> HistoryFuture<'_, Arc<HistorySnapshot>> {
        Box::pin(async move {
            // Row text is held to the live capture's own cap; allocation
            // overhead is counted once, in `bytes`, against the total budget.
            if snapshot.row_bytes() > MAX_PANE_BYTES {
                return Err(HistoryError::TooLarge);
            }
            let bytes = snapshot.bytes();
            let mut snapshots = self
                .snapshots
                .lock()
                .map_err(|_| HistoryError::Unavailable)?;
            self.prune(&mut snapshots)?;
            // Concurrent first pages of one unchanged capture share a copy.
            if let Some(held) = snapshots.values().find(|held| {
                held.scope == snapshot.scope
                    && held.epoch == snapshot.epoch
                    && held.base == snapshot.base
                    && held.rows.len() == snapshot.rows.len()
            }) {
                return Ok(held.clone());
            }
            loop {
                let total = snapshots.values().map(|snap| snap.bytes()).sum::<usize>();
                let session_count = snapshots
                    .values()
                    .filter(|snap| snap.scope.session == snapshot.scope.session)
                    .count();
                let pane_count = snapshots
                    .values()
                    .filter(|snap| {
                        snap.scope.session == snapshot.scope.session
                            && snap.scope.pane == snapshot.scope.pane
                    })
                    .count();
                if pane_count < MAX_PANE_SNAPSHOTS
                    && session_count < MAX_SESSION_SNAPSHOTS
                    && snapshots.len() < MAX_SNAPSHOTS
                    && total + bytes <= MAX_TOTAL_BYTES
                {
                    break;
                }
                let oldest = snapshots
                    .iter()
                    .filter(|(_, snap)| {
                        if pane_count >= MAX_PANE_SNAPSHOTS {
                            snap.scope.session == snapshot.scope.session
                                && snap.scope.pane == snapshot.scope.pane
                        } else if session_count >= MAX_SESSION_SNAPSHOTS {
                            snap.scope.session == snapshot.scope.session
                        } else {
                            true
                        }
                    })
                    .min_by_key(|(_, snap)| snap.created)
                    .map(|(id, _)| *id);
                if let Some(id) = oldest {
                    snapshots.remove(&id);
                } else {
                    return Err(HistoryError::TooLarge);
                }
            }
            let snapshot = Arc::new(snapshot);
            snapshots.insert(snapshot.id, snapshot.clone());
            Ok(snapshot)
        })
    }

    fn remove(&self, id: Uuid) -> HistoryFuture<'_, ()> {
        Box::pin(async move {
            self.snapshots
                .lock()
                .map_err(|_| HistoryError::Unavailable)?
                .remove(&id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::history::{parse_cursor, read_page, MAX_PAGE_BYTES};
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn scope(session: &str, pane: &str, limit: usize) -> HistoryScope {
        HistoryScope {
            session: session.into(),
            pane: pane.into(),
            format: "text".into(),
            device: "d".into(),
            generation: "g".into(),
            limit,
        }
    }

    fn feed(repository: &MemoryHistoryRepository, scope: &HistoryScope, rows: &[String]) {
        let mut captures = repository.captures.lock().unwrap();
        captures.observe(
            &scope.session,
            &json!({"pane_id": scope.pane, "scroll": {"max_offset_from_bottom": 0}}),
        );
        captures.record_frame(
            &scope.session,
            &scope.pane,
            "recent_unwrapped",
            "text",
            &rows.join("\n"),
        );
        captures.record_frame(
            &scope.session,
            &scope.pane,
            "recent_unwrapped",
            "text",
            "live 1\nlive 2\nlive 3\nlive 4",
        );
    }

    #[tokio::test]
    async fn memory_history_quota_ttl_and_capture_epoch_pruning_are_explicit() {
        let repository =
            MemoryHistoryRepository::new(Arc::new(Mutex::new(ScrollbackStore::default())));
        let scope = scope("s", "p", 2);
        feed(
            &repository,
            &scope,
            &(0..7).map(|i| format!("row {i}")).collect::<Vec<_>>(),
        );
        let first = read_page(&repository, scope.clone(), None).await.unwrap();
        let cursor = first["next_before"].as_str().unwrap();
        let (id, _) = parse_cursor(cursor).unwrap();
        Arc::get_mut(repository.snapshots.lock().unwrap().get_mut(&id).unwrap())
            .unwrap()
            .created = Instant::now() - SNAPSHOT_TTL;
        assert_eq!(
            read_page(&repository, scope.clone(), Some(cursor)).await,
            Err(HistoryError::Gone)
        );
        let first = read_page(&repository, scope.clone(), None).await.unwrap();
        for limit in [3, 4] {
            let mut other = scope.clone();
            other.limit = limit;
            read_page(&repository, other, None).await.unwrap();
        }
        assert_eq!(
            repository.snapshots.lock().unwrap().len(),
            MAX_PANE_SNAPSHOTS
        );
        assert_eq!(
            read_page(&repository, scope.clone(), first["next_before"].as_str()).await,
            Err(HistoryError::Gone)
        );
        let first = read_page(&repository, scope.clone(), None).await.unwrap();
        repository
            .captures
            .lock()
            .unwrap()
            .observe_listing("s", &json!({"panes": []}));
        assert_eq!(
            read_page(&repository, scope, first["next_before"].as_str()).await,
            Err(HistoryError::Gone)
        );
        assert!(repository.snapshots.lock().unwrap().is_empty());
    }

    /// Rows of `size` bytes, numbered from `from`, as one read.
    fn rows(from: usize, count: usize, size: usize) -> String {
        (from..from + count)
            .map(|i| format!("{i:08} {}", "x".repeat(size - 9)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn frame(repository: &MemoryHistoryRepository, scope: &HistoryScope, text: &str) {
        repository.captures.lock().unwrap().record_frame(
            &scope.session,
            &scope.pane,
            "recent_unwrapped",
            "text",
            text,
        );
    }

    #[tokio::test]
    async fn a_pane_at_its_byte_cap_pages_and_keeps_paging_while_it_trims() {
        let repository =
            MemoryHistoryRepository::new(Arc::new(Mutex::new(ScrollbackStore::default())));
        let scope = scope("s", "busy", 500);
        // More than the cap in 1 KiB rows: the live buffer trims to exactly
        // its 2 MiB, which used to answer 413 because the row overhead was
        // counted on top of a budget the rows alone already filled.
        let size = 1024;
        let count = MAX_PANE_BYTES / size + 200;
        feed(
            &repository,
            &scope,
            &rows(0, count, size)
                .split('\n')
                .map(String::from)
                .collect::<Vec<_>>(),
        );
        let pane = |width: u32| {
            json!({"pane_id": scope.pane, "width": width, "height": 40,
                   "scroll": {"max_offset_from_bottom": 0}})
        };
        repository
            .captures
            .lock()
            .unwrap()
            .observe(&scope.session, &pane(80));
        frame(&repository, &scope, &rows(500_000, 4, size));
        let mut answer = read_page(&repository, scope.clone(), None).await.unwrap();
        let first_id = answer["snapshot_id"].clone();
        let mut cursors = Vec::new();
        while let Some(cursor) = answer["next_before"].as_str() {
            cursors.push(cursor.to_owned());
            answer = read_page(&repository, scope.clone(), Some(cursor))
                .await
                .unwrap();
        }
        assert!(cursors.len() >= 4, "{}", cursors.len());

        // The pane keeps producing output: each new screen trims a few rows
        // off the front -- here 8, which may empty the short oldest page and
        // reach into the one after it. Every page whose rows are still held
        // keeps serving, under the same epoch.
        frame(&repository, &scope, &rows(1_000_000, 4, size));
        frame(&repository, &scope, &rows(2_000_000, 4, size));
        for cursor in &cursors[..cursors.len() - 2] {
            read_page(&repository, scope.clone(), Some(cursor))
                .await
                .unwrap();
        }
        // Mid-traversal, a new traversal is not handed the old snapshot: the
        // capture has moved, so its first page is the current newest rows.
        let restarted = read_page(&repository, scope.clone(), None).await.unwrap();
        assert_ne!(restarted["snapshot_id"], first_id);
        assert!(
            newest(&restarted).starts_with("01000003 "),
            "{}",
            newest(&restarted)
        );

        // Enough output to trim the oldest page away: that cursor is 410, and
        // the snapshot goes with it instead of waiting out its TTL.
        frame(&repository, &scope, &rows(3_000_000, 400, size));
        frame(&repository, &scope, &rows(4_000_000, 4, size));
        let mut stale = restarted;
        let mut oldest = None;
        while let Some(cursor) = stale["next_before"].as_str().map(String::from) {
            match read_page(&repository, scope.clone(), Some(&cursor)).await {
                Ok(page) => stale = page,
                Err(error) => {
                    oldest = Some(error);
                    break;
                }
            }
        }
        assert_eq!(oldest, Some(HistoryError::Gone));

        // Starting again, as the 410 says, gives a fresh snapshot of the
        // current rows, and that walk completes.
        let mut answer = read_page(&repository, scope.clone(), None).await.unwrap();
        assert!(
            newest(&answer).starts_with("03000399 "),
            "{}",
            newest(&answer)
        );
        let fresh = answer["next_before"].as_str().unwrap().to_owned();
        let mut pages = 1;
        while let Some(cursor) = answer["next_before"].as_str().map(String::from) {
            answer = read_page(&repository, scope.clone(), Some(&cursor))
                .await
                .unwrap();
            pages += 1;
        }
        assert!(pages >= 4, "{pages}");

        // A resize is a genuine reset: every cursor of the traversal is 410.
        repository
            .captures
            .lock()
            .unwrap()
            .observe(&scope.session, &pane(120));
        assert_eq!(
            read_page(&repository, scope.clone(), Some(&fresh)).await,
            Err(HistoryError::Gone)
        );
    }

    /// The newest row a page carries.
    fn newest(page: &serde_json::Value) -> String {
        page["rows"]
            .as_array()
            .and_then(|rows| rows.last())
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[tokio::test]
    async fn memory_history_global_session_bytes_counts_and_oversized_rows_are_bounded() {
        let repository =
            MemoryHistoryRepository::new(Arc::new(Mutex::new(ScrollbackStore::default())));
        for large in [true, false] {
            for i in 0..50 {
                let scope = scope(&format!("s{}", i / 12), &format!("p{i}"), 2);
                let rows = if large {
                    vec!["x".repeat(200_000); 5]
                } else {
                    vec!["a".into(), "b".into(), "c".into()]
                };
                feed(&repository, &scope, &rows);
                read_page(&repository, scope, None).await.unwrap();
                let snapshots = repository.snapshots.lock().unwrap();
                assert!(snapshots.len() <= MAX_SNAPSHOTS);
                assert!(
                    snapshots.values().map(|snap| snap.bytes()).sum::<usize>() <= MAX_TOTAL_BYTES
                );
                for snap in snapshots.values() {
                    assert!(snap.row_bytes() <= MAX_PANE_BYTES);
                    assert!(
                        snapshots
                            .values()
                            .filter(|other| other.scope.session == snap.scope.session)
                            .count()
                            <= MAX_SESSION_SNAPSHOTS
                    );
                }
            }
        }
        assert_eq!(repository.snapshots.lock().unwrap().len(), MAX_SNAPSHOTS);
        let scope = scope("huge", "p", 2);
        feed(&repository, &scope, &["x".repeat(MAX_PAGE_BYTES)]);
        assert_eq!(
            read_page(&repository, scope, None).await,
            Err(HistoryError::TooLarge)
        );
    }
}
