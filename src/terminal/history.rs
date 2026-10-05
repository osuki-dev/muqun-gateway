//! Captured-history application contract. Pagination knows no database internals.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) const MAX_HISTORY_LIMIT: usize = 500;
pub(crate) const MAX_PAGE_BYTES: usize = 256 * 1024;
pub(crate) const SNAPSHOT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryScope {
    pub session: String,
    pub pane: String,
    pub format: String,
    pub device: String,
    pub generation: String,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryError {
    InvalidCursor,
    Gone,
    Mismatch,
    TooLarge,
    Unavailable,
}

/// Folded historical prefix in one exact read shape; never the mutable viewport.
#[derive(Debug)]
pub(crate) struct Capture {
    pub epoch: Uuid,
    /// Rows the capture has dropped off its front so far. `rows[0]` is row
    /// `trimmed` of the capture; a trim moves this, never `epoch`.
    pub trimmed: u64,
    /// How many historical rows the capture holds now, whether or not
    /// `rows` was asked for: with `trimmed`, where its newest row is.
    pub len: usize,
    pub rows: Vec<String>,
}

/// A pinned traversal is ephemeral even when a future adapter persists captures.
#[derive(Debug)]
pub(crate) struct HistorySnapshot {
    pub id: Uuid,
    pub scope: HistoryScope,
    pub epoch: Uuid,
    /// The capture's `trimmed` when this was pinned: `rows[i]` is capture row
    /// `base + i`.
    pub base: u64,
    pub created: Instant,
    pub rows: Vec<String>,
    // Tokens identify application-created intervals, never client-chosen offsets.
    pages: Vec<(Uuid, usize, usize)>,
}

pub(crate) type HistoryFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, HistoryError>> + Send + 'a>>;

/// The changing storage boundary: retained captures and bounded pinned traversals.
/// An adapter may perform async I/O; no caller lends it a capture mutex guard.
/// `pin` atomically reuses a still-valid snapshot for the same scope/epoch, so
/// concurrent first-page requests do not create duplicate copies in storage.
pub(crate) trait HistoryRepository: Send + Sync {
    fn capture<'a>(
        &'a self,
        scope: &'a HistoryScope,
        include_rows: bool,
    ) -> HistoryFuture<'a, Option<Capture>>;
    fn find<'a>(
        &'a self,
        scope: &'a HistoryScope,
    ) -> HistoryFuture<'a, Option<Arc<HistorySnapshot>>>;
    fn load(&self, id: Uuid) -> HistoryFuture<'_, Option<Arc<HistorySnapshot>>>;
    fn pin(&self, snapshot: HistorySnapshot) -> HistoryFuture<'_, Arc<HistorySnapshot>>;
    fn remove(&self, id: Uuid) -> HistoryFuture<'_, ()>;
}

/// Stable application use case. Backend liveness is established before calling;
/// storage failures are explicit, and no storage coordinates enter the wire.
pub(crate) async fn read_page(
    repository: &dyn HistoryRepository,
    scope: HistoryScope,
    before: Option<&str>,
) -> Result<Value, HistoryError> {
    if let Some(cursor) = before {
        let (id, token) = parse_cursor(cursor)?;
        let snapshot = repository.load(id).await?.ok_or(HistoryError::Gone)?;
        if snapshot.scope != scope {
            return Err(HistoryError::Mismatch);
        }
        let page = snapshot
            .pages
            .iter()
            .position(|(held, _, _)| *held == token)
            .ok_or(HistoryError::Gone)?;
        return match validity(repository, &scope, &snapshot, Some(page)).await? {
            Validity::Valid => Ok(render(&snapshot, page)),
            // A 410 tells the client to start again without a cursor, so the
            // snapshot goes now: its rows are no longer the capture's.
            Validity::Moved | Validity::Reset => {
                repository.remove(id).await?;
                Err(HistoryError::Gone)
            }
        };
    }
    // A new traversal starts from the capture as it is now. A pinned snapshot
    // is reused only while the capture has not moved since -- same epoch,
    // nothing trimmed, nothing appended -- so a client restarting after a 410
    // never walks into the same stale snapshot again. One that has moved is
    // dropped here, not left to its TTL.
    if let Some(snapshot) = repository.find(&scope).await? {
        if validity(repository, &scope, &snapshot, None).await? == Validity::Valid {
            return Ok(render(&snapshot, 0));
        }
        repository.remove(snapshot.id).await?;
    }
    let Some(capture) = repository.capture(&scope, true).await? else {
        return Ok(empty(&scope, "not_captured"));
    };
    if capture.rows.is_empty() {
        return Ok(empty(&scope, "captured"));
    }
    let snapshot = HistorySnapshot::new(scope, capture)?;
    let snapshot = repository.pin(snapshot).await?;
    // A capture may reset while an async adapter pins it. Fail closed rather
    // than returning rows from the old incarnation after that boundary.
    if validity(repository, &snapshot.scope, &snapshot, Some(0)).await? != Validity::Valid {
        repository.remove(snapshot.id).await?;
        return Err(HistoryError::Gone);
    }
    Ok(render(&snapshot, 0))
}

#[derive(Debug, PartialEq, Eq)]
enum Validity {
    Valid,
    /// Same capture, moved on: for a cursor, its page's rows have been
    /// trimmed off the front (serving them would show rows the capture no
    /// longer has); for a new traversal, anything was trimmed or appended.
    Moved,
    /// Expired, or the capture was reset (resize, mode flip, replacement,
    /// eviction, disappearance): every cursor of the snapshot is gone.
    Reset,
}

/// Whether a pinned snapshot still serves `Some(page)` of a traversal, or
/// (`None`) can stand in for a new one. A capture trimmed at its size cap
/// keeps its epoch, so a busy pane does not invalidate a traversal on every
/// frame; only the pages whose rows were trimmed away go.
async fn validity(
    repository: &dyn HistoryRepository,
    scope: &HistoryScope,
    snapshot: &HistorySnapshot,
    page: Option<usize>,
) -> Result<Validity, HistoryError> {
    if snapshot.created.elapsed() >= SNAPSHOT_TTL {
        return Ok(Validity::Reset);
    }
    let Some(capture) = repository.capture(scope, false).await? else {
        return Ok(Validity::Reset);
    };
    if capture.epoch != snapshot.epoch {
        return Ok(Validity::Reset);
    }
    let current = match page {
        Some(page) => snapshot.base + snapshot.pages[page].1 as u64 >= capture.trimmed,
        None => snapshot.is_current(&capture),
    };
    Ok(if current {
        Validity::Valid
    } else {
        Validity::Moved
    })
}

impl HistorySnapshot {
    pub(crate) fn new(scope: HistoryScope, capture: Capture) -> Result<Self, HistoryError> {
        if !(1..=MAX_HISTORY_LIMIT).contains(&scope.limit)
            || capture.rows.len() > super::scrollback::MAX_PANE_LINES
        {
            return Err(HistoryError::TooLarge);
        }
        let mut pages = Vec::new();
        let mut end = capture.rows.len();
        while end > 0 {
            let mut start = end;
            let mut bytes = 0;
            while start > 0 && end - start < scope.limit {
                let next = capture.rows[start - 1].len() + 1;
                if next > MAX_PAGE_BYTES {
                    return Err(HistoryError::TooLarge);
                }
                if bytes + next > MAX_PAGE_BYTES {
                    break;
                }
                bytes += next;
                start -= 1;
            }
            pages.push((Uuid::new_v4(), start, end));
            end = start;
        }
        Ok(Self {
            id: Uuid::new_v4(),
            scope,
            epoch: capture.epoch,
            base: capture.trimmed,
            created: Instant::now(),
            rows: capture.rows,
            pages,
        })
    }

    /// Pinned from exactly this state of the capture: nothing has been
    /// trimmed or appended since.
    pub(crate) fn is_current(&self, capture: &Capture) -> bool {
        capture.epoch == self.epoch
            && capture.trimmed == self.base
            && capture.len == self.rows.len()
    }

    /// Row text alone: the measure the live capture is capped by.
    pub(crate) fn row_bytes(&self) -> usize {
        self.rows.iter().map(String::len).sum()
    }

    /// Everything the snapshot allocates, for the store's total budget.
    pub(crate) fn bytes(&self) -> usize {
        self.rows.iter().map(String::capacity).sum::<usize>()
            + self.rows.capacity() * std::mem::size_of::<String>()
            + self.pages.capacity() * std::mem::size_of::<(Uuid, usize, usize)>()
            + self.scope.session.capacity()
            + self.scope.pane.capacity()
            + self.scope.format.capacity()
            + self.scope.device.capacity()
            + self.scope.generation.capacity()
            + std::mem::size_of::<Self>()
    }
}

pub(crate) fn parse_cursor(cursor: &str) -> Result<(Uuid, Uuid), HistoryError> {
    if cursor.len() != 65
        || cursor.as_bytes()[32] != b'.'
        || !cursor
            .bytes()
            .enumerate()
            .all(|(i, c)| i == 32 || c.is_ascii_hexdigit())
    {
        return Err(HistoryError::InvalidCursor);
    }
    Ok((
        Uuid::parse_str(&cursor[..32]).map_err(|_| HistoryError::InvalidCursor)?,
        Uuid::parse_str(&cursor[33..]).map_err(|_| HistoryError::InvalidCursor)?,
    ))
}

fn empty(scope: &HistoryScope, availability: &str) -> Value {
    json!({
        "session_id": scope.session, "pane_id": scope.pane,
        "generation": scope.generation, "source": "gateway-captured",
        "read_source": "recent-unwrapped", "format": scope.format,
        "availability": availability, "snapshot_id": null, "capture_epoch": null,
        "rows": [], "row_count": 0, "has_more": false, "next_before": null,
        "order": "oldest-to-newest", "includes_live_viewport": false,
        "complete_archive": false
    })
}

fn render(snapshot: &HistorySnapshot, page: usize) -> Value {
    let (_, start, end) = snapshot.pages[page];
    let next = snapshot
        .pages
        .get(page + 1)
        .map(|(token, _, _)| format!("{}.{}", snapshot.id.simple(), token.simple()));
    let mut answer = empty(&snapshot.scope, "captured");
    answer["snapshot_id"] = json!(snapshot.id.to_string());
    answer["capture_epoch"] = json!(snapshot.epoch.to_string());
    answer["rows"] = json!(snapshot.rows[start..end]);
    answer["row_count"] = json!(end - start);
    answer["has_more"] = json!(next.is_some());
    answer["next_before"] = json!(next);
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn scope() -> HistoryScope {
        HistoryScope {
            session: "s".into(),
            pane: "p".into(),
            format: "text".into(),
            device: "d".into(),
            generation: "g".into(),
            limit: 2,
        }
    }

    // A deliberately asynchronous fake: application code cannot depend on the
    // memory adapter's ready futures, storage locks or dictionary representation.
    struct FakeRepository {
        capture: Mutex<Option<(Uuid, Vec<String>)>>,
        trimmed: Mutex<u64>,
        snapshots: Mutex<HashMap<Uuid, Arc<HistorySnapshot>>>,
        failed: bool,
    }
    impl FakeRepository {
        fn new() -> Self {
            Self {
                capture: Mutex::new(Some((
                    Uuid::new_v4(),
                    (0..7).map(|i| format!("row {i}")).collect(),
                ))),
                trimmed: Mutex::new(0),
                snapshots: Mutex::new(HashMap::new()),
                failed: false,
            }
        }
    }
    impl HistoryRepository for FakeRepository {
        fn capture<'a>(
            &'a self,
            _: &'a HistoryScope,
            include_rows: bool,
        ) -> HistoryFuture<'a, Option<Capture>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                if self.failed {
                    return Err(HistoryError::Unavailable);
                }
                Ok(self
                    .capture
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|(epoch, rows)| Capture {
                        epoch: *epoch,
                        trimmed: *self.trimmed.lock().unwrap(),
                        len: rows.len(),
                        rows: if include_rows { rows.clone() } else { vec![] },
                    }))
            })
        }
        fn find<'a>(
            &'a self,
            scope: &'a HistoryScope,
        ) -> HistoryFuture<'a, Option<Arc<HistorySnapshot>>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(self
                    .snapshots
                    .lock()
                    .unwrap()
                    .values()
                    .find(|snap| snap.scope == *scope)
                    .cloned())
            })
        }
        fn load(&self, id: Uuid) -> HistoryFuture<'_, Option<Arc<HistorySnapshot>>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(self.snapshots.lock().unwrap().get(&id).cloned())
            })
        }
        fn pin(&self, snapshot: HistorySnapshot) -> HistoryFuture<'_, Arc<HistorySnapshot>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                let mut snapshots = self.snapshots.lock().unwrap();
                if let Some(held) = snapshots.values().find(|held| {
                    held.scope == snapshot.scope
                        && held.epoch == snapshot.epoch
                        && held.base == snapshot.base
                        && held.rows == snapshot.rows
                }) {
                    return Ok(held.clone());
                }
                let snapshot = Arc::new(snapshot);
                snapshots.insert(snapshot.id, snapshot.clone());
                Ok(snapshot)
            })
        }
        fn remove(&self, id: Uuid) -> HistoryFuture<'_, ()> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.snapshots.lock().unwrap().remove(&id);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn history_use_case_tiles_and_replays_frozen_pages_through_async_port() {
        let repository = FakeRepository::new();
        let first = read_page(&repository, scope(), None).await.unwrap();
        assert_eq!(first["rows"], json!(["row 5", "row 6"]));
        repository
            .capture
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .1
            .push("appended".into());
        // An unchanged capture reuses its snapshot; a moved one does not.
        let fresh = read_page(&repository, scope(), None).await.unwrap();
        assert_ne!(fresh["snapshot_id"], first["snapshot_id"]);
        assert_eq!(fresh["rows"], json!(["row 6", "appended"]));
        assert_eq!(read_page(&repository, scope(), None).await.unwrap(), fresh);
        repository.capture.lock().unwrap().as_mut().unwrap().1.pop();
        let first = read_page(&repository, scope(), None).await.unwrap();
        let mut answer = first;
        let mut tiled = Vec::new();
        loop {
            let mut page: Vec<String> = serde_json::from_value(answer["rows"].clone()).unwrap();
            page.extend(tiled);
            tiled = page;
            let Some(cursor) = answer["next_before"].as_str() else {
                break;
            };
            let next = read_page(&repository, scope(), Some(cursor)).await.unwrap();
            assert_eq!(
                next,
                read_page(&repository, scope(), Some(cursor)).await.unwrap()
            );
            answer = next;
        }
        assert_eq!(
            tiled,
            (0..7).map(|i| format!("row {i}")).collect::<Vec<_>>()
        );
        assert_eq!(repository.snapshots.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn history_use_case_binds_scope_and_invalidates_epochs_restart_and_ttl() {
        let repository = FakeRepository::new();
        let first = read_page(&repository, scope(), None).await.unwrap();
        let cursor = first["next_before"].as_str().unwrap();
        for field in 0..6 {
            let mut other = scope();
            match field {
                0 => other.session = "other".into(),
                1 => other.pane = "other".into(),
                2 => other.format = "ansi".into(),
                3 => other.device = "other".into(),
                4 => other.generation = "other".into(),
                _ => other.limit = 3,
            }
            assert_eq!(
                read_page(&repository, other, Some(cursor)).await,
                Err(HistoryError::Mismatch)
            );
        }
        let (id, _) = parse_cursor(cursor).unwrap();
        let forged = format!("{}.{}", id.simple(), Uuid::new_v4().simple());
        assert_eq!(
            read_page(&repository, scope(), Some(&forged)).await,
            Err(HistoryError::Gone)
        );
        assert_eq!(
            read_page(&FakeRepository::new(), scope(), Some(cursor)).await,
            Err(HistoryError::Gone)
        );
        repository.capture.lock().unwrap().as_mut().unwrap().0 = Uuid::new_v4();
        assert_eq!(
            read_page(&repository, scope(), Some(cursor)).await,
            Err(HistoryError::Gone)
        );
        let fresh = read_page(&repository, scope(), None).await.unwrap();
        let cursor = fresh["next_before"].as_str().unwrap();
        let (id, _) = parse_cursor(cursor).unwrap();
        Arc::get_mut(repository.snapshots.lock().unwrap().get_mut(&id).unwrap())
            .unwrap()
            .created = Instant::now() - SNAPSHOT_TTL;
        assert_eq!(
            read_page(&repository, scope(), Some(cursor)).await,
            Err(HistoryError::Gone)
        );
    }

    #[tokio::test]
    async fn a_trim_expires_only_the_cursors_whose_rows_it_dropped() {
        // Seven rows, pages of two: [5,6] [3,4] [1,2] [0].
        let repository = FakeRepository::new();
        let mut cursors = Vec::new();
        let mut answer = read_page(&repository, scope(), None).await.unwrap();
        while let Some(cursor) = answer["next_before"].as_str() {
            cursors.push(cursor.to_owned());
            answer = read_page(&repository, scope(), Some(cursor)).await.unwrap();
        }
        assert_eq!(cursors.len(), 3);
        // The capture drops rows 0 and 1 off its front, same epoch: pages
        // whose rows are still held keep serving.
        *repository.trimmed.lock().unwrap() = 2;
        repository
            .capture
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .1
            .drain(..2);
        assert_eq!(
            read_page(&repository, scope(), Some(&cursors[0]))
                .await
                .unwrap()["rows"],
            json!(["row 3", "row 4"])
        );
        // The page holding rows 1-2 is gone, and with it the snapshot: the
        // client starts again without a cursor and must not land in it.
        assert_eq!(
            read_page(&repository, scope(), Some(&cursors[1])).await,
            Err(HistoryError::Gone)
        );
        assert!(repository.snapshots.lock().unwrap().is_empty());
        assert_eq!(
            read_page(&repository, scope(), Some(&cursors[0])).await,
            Err(HistoryError::Gone)
        );
        let mut answer = read_page(&repository, scope(), None).await.unwrap();
        let mut tiled = Vec::new();
        loop {
            let mut page: Vec<String> = serde_json::from_value(answer["rows"].clone()).unwrap();
            page.extend(tiled);
            tiled = page;
            let Some(cursor) = answer["next_before"].as_str() else {
                break;
            };
            answer = read_page(&repository, scope(), Some(cursor)).await.unwrap();
        }
        assert_eq!(
            tiled,
            (2..7).map(|i| format!("row {i}")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn history_use_case_distinguishes_empty_storage_failure_and_size_limits() {
        for malformed in [
            "",
            "x",
            &"x".repeat(10000),
            &"é".repeat(32),
            "00000000000000000000000000000000/00000000000000000000000000000000",
        ] {
            assert_eq!(parse_cursor(malformed), Err(HistoryError::InvalidCursor));
        }
        let mut repository = FakeRepository::new();
        *repository.capture.lock().unwrap() = None;
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["availability"],
            "not_captured"
        );
        repository.failed = true;
        assert_eq!(
            read_page(&repository, scope(), None).await,
            Err(HistoryError::Unavailable)
        );
        repository.failed = false;
        *repository.capture.lock().unwrap() =
            Some((Uuid::new_v4(), vec!["x".repeat(MAX_PAGE_BYTES)]));
        assert_eq!(
            read_page(&repository, scope(), None).await,
            Err(HistoryError::TooLarge)
        );
        *repository.capture.lock().unwrap() =
            Some((Uuid::new_v4(), vec!["x".repeat(MAX_PAGE_BYTES / 2); 3]));
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["row_count"],
            1
        );
    }
}
