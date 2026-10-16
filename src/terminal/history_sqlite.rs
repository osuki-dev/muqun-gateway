//! Optional captured-history checkpoints, kept in the shared store under the
//! `history` namespace. The live fold coalesces revisions; when the fold's
//! version moves, a Tokio task hands the store worker one pass that takes at
//! most one bounded projection's shared rows under the fold lock and encodes
//! and writes it without that lock. An idle fold wakes it only to prune.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::history::{
    Capture, HistoryError, HistoryFuture, HistoryRepository, HistoryScope, HistorySnapshot,
    StorageStats,
};
use super::history_memory::MemoryHistoryRepository;
use super::history_segments::{Segment, SegmentPosition, MAX_SEGMENTS, RETENTION_MS};
use super::scrollback::{ProjectionStamp, ScrollbackStore, MAX_PANE_BYTES, MAX_PANE_LINES};
use crate::platform::config::{Config, HistoryStorage};
use crate::platform::store::sqlite::{Migration, SqliteStore};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "history",
    steps: &[
        "
CREATE TABLE history_backends(session TEXT PRIMARY KEY, endpoint TEXT NOT NULL, incarnation TEXT NOT NULL);
CREATE TABLE history_captures(identity TEXT PRIMARY KEY, policy TEXT NOT NULL, rows BLOB NOT NULL,
  bytes INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE INDEX history_capture_retention ON history_captures(updated);
",
        // Quotas read `bytes` and `updated` from this covering index instead of
        // walking each row's multi-MiB BLOB; it also serves age pruning.
        "
CREATE INDEX history_capture_quota ON history_captures(updated, identity, bytes);
DROP INDEX history_capture_retention;
",
        // Parent rows continue to count read shapes, not segments. The legacy
        // binary payload is retained verbatim and validated by the same decoder
        // before adoption. A corrupt record can never authorize empty recovery.
        "
CREATE TABLE history_segments(identity TEXT NOT NULL REFERENCES history_captures(identity) ON DELETE CASCADE,
  segment_id TEXT NOT NULL, position TEXT NOT NULL, rows BLOB NOT NULL,
  bytes INTEGER NOT NULL, changed_ms INTEGER NOT NULL, PRIMARY KEY(identity,segment_id));
CREATE INDEX history_segment_age ON history_segments(changed_ms);
INSERT INTO history_segments SELECT identity, lower(hex(randomblob(16))),
  json_object('segment_id', '00000000-0000-0000-0000-000000000000', 'started_at_ms', updated*1000,
    'sealed_at_ms', updated*1000, 'boundary_reason', 'gateway-restart', 'base', 0, 'len', 0),
  rows, bytes, updated*1000 FROM history_captures WHERE length(rows)>0;
UPDATE history_captures SET rows=x'';
UPDATE history_segments SET bytes=bytes+length(CAST(position AS BLOB))+length(segment_id)+16;
UPDATE history_captures SET bytes=(SELECT coalesce(sum(bytes),0) FROM history_segments WHERE history_segments.identity=history_captures.identity)+length(CAST(identity AS BLOB))+length(CAST(policy AS BLOB))+64;
",
    ],
};

const MAX_RECORDS: i64 = 48;
const MAX_PAYLOAD_BYTES: i64 = 24 * 1024 * 1024;
// The pane cap measures row text, just like the live fold and snapshot cache.
// Length prefixes have their own bounded overhead, counted in payload quotas.
const MAX_ENCODED_BYTES: usize = MAX_PANE_BYTES + 4 * MAX_PANE_LINES;
const RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
/// How soon a pass that left work (a debounced scope, a projection still to
/// write) is followed by the next.
const FLUSH_TICK: Duration = Duration::from_millis(100);
const PRUNE_EVERY: Duration = Duration::from_secs(60);
/// A written scope waits this long before its next ordinary write; a reset,
/// an identity change or an emptied projection is written at once.
const WRITE_DEBOUNCE: Duration = Duration::from_secs(3);
/// Checkpoint reads waiting on the shared store; one more is a 503.
const MAX_PENDING_READS: usize = 16;
/// A WAL past this is folded back into the database after the pass that
/// grew it -- below SQLite's own autocheckpoint (1,000 pages, about 4 MiB),
/// which folds pages back but never shrinks the file; TRUNCATE does.
const WAL_CHECKPOINT_BYTES: u64 = 1024 * 1024;
/// A writer that has written nothing for this long folds the WAL back on its
/// next prune pass, so an idle store's WAL file is empty.
const IDLE_CHECKPOINT: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(60)
};

/// How long the write loop waits after a failed pass before it tries again:
/// each failure moves one step on, and a pass that works starts over.
const RETRY_AFTER: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

pub(crate) struct SqliteHistoryRepository {
    memory: MemoryHistoryRepository,
    captures: Arc<Mutex<ScrollbackStore>>,
    /// `None` until initialization works; a failed one is retried on the
    /// write loop's backoff ([`RETRY_AFTER`]).
    checkpoints: Arc<std::sync::RwLock<Option<Arc<Checkpoints>>>>,
    reads: Semaphore,
    /// The write loop, or the initialization retrying before it.
    flusher: Option<tokio::task::JoinHandle<()>>,
}

/// The shared store's migration creates the `history` tables either way;
/// memory selection only skips every runtime read and write of them.
pub(crate) async fn repository(
    config: &Config,
    db: Arc<SqliteStore>,
    captures: Arc<Mutex<ScrollbackStore>>,
) -> Arc<dyn HistoryRepository> {
    match config.history.storage {
        HistoryStorage::Memory => Arc::new(MemoryHistoryRepository::new(captures)),
        HistoryStorage::Sqlite => {
            Arc::new(SqliteHistoryRepository::open(db, config, captures).await)
        }
    }
}

impl SqliteHistoryRepository {
    pub(super) async fn open(
        db: Arc<SqliteStore>,
        config: &Config,
        captures: Arc<Mutex<ScrollbackStore>>,
    ) -> Self {
        Self::open_with(db, config, captures, WRITE_DEBOUNCE).await
    }

    async fn open_with(
        db: Arc<SqliteStore>,
        config: &Config,
        captures: Arc<Mutex<ScrollbackStore>>,
        debounce: Duration,
    ) -> Self {
        let memory = MemoryHistoryRepository::new(captures.clone());
        let slot = Arc::new(std::sync::RwLock::new(None));
        let opened = Checkpoints::open(db.clone(), config, captures.clone(), debounce).await;
        let flusher = match opened {
            Ok(checkpoints) => {
                if let Ok(mut slot) = slot.write() {
                    *slot = Some(checkpoints.clone());
                }
                tokio::spawn(checkpoints.flush_forever())
            }
            Err(error) => {
                tracing::error!(%error, "captured history initialization failed; retrying, realtime output remains available");
                let (slot, config, captures) = (slot.clone(), config.clone(), captures.clone());
                tokio::spawn(async move {
                    for failures in 0usize.. {
                        tokio::time::sleep(RETRY_AFTER[failures.min(RETRY_AFTER.len() - 1)]).await;
                        match Checkpoints::open(db.clone(), &config, captures.clone(), debounce)
                            .await
                        {
                            Ok(checkpoints) => {
                                tracing::info!("captured history storage initialized");
                                if let Ok(mut slot) = slot.write() {
                                    *slot = Some(checkpoints.clone());
                                }
                                return checkpoints.flush_forever().await;
                            }
                            Err(error) => {
                                tracing::debug!(%error, "captured history initialization failed again")
                            }
                        }
                    }
                })
            }
        };
        Self {
            memory,
            captures,
            checkpoints: slot,
            reads: Semaphore::new(MAX_PENDING_READS),
            flusher: Some(flusher),
        }
    }

    /// The store, once initialized.
    fn checkpoints(&self) -> Option<Arc<Checkpoints>> {
        self.checkpoints.read().ok()?.clone()
    }

    /// The store, while its last operation worked. Only what needs it --
    /// recovering a checkpoint -- asks; what memory holds is served either way.
    fn health(&self) -> Result<Arc<Checkpoints>, HistoryError> {
        self.checkpoints()
            .filter(|checkpoints| checkpoints.healthy.load(Ordering::Acquire))
            .ok_or(HistoryError::Unavailable)
    }

    /// Stop the write loop, then write every dirty projection, debounce or not.
    #[cfg(test)]
    pub(super) async fn close(mut self) {
        if let Some(flusher) = self.flusher.take() {
            flusher.abort();
            let _ = flusher.await;
        }
        if let Ok(checkpoints) = self.health() {
            for _ in 0..16 {
                if !matches!(
                    checkpoints.flush_with(now_seconds(), Duration::ZERO).await,
                    Ok(false)
                ) {
                    break;
                }
            }
        }
    }

    /// Adopt the pane's checkpoint, when one could be read: a verified,
    /// unreset pane. Eligibility first, from the fold alone -- a pane that
    /// could never have one answers from memory (`not_captured`, an empty
    /// capture) whatever the store's health; only a read the store must
    /// serve answers 503 while it fails.
    async fn recover(&self, scope: &HistoryScope) -> Result<(), HistoryError> {
        let stamp = self
            .captures
            .lock()
            .map_err(|_| HistoryError::Unavailable)?
            .projection_stamp(scope);
        let Some(stamp) = stamp.filter(|stamp| {
            self.captures
                .lock()
                .is_ok_and(|c| c.recovery_pending(stamp))
        }) else {
            return Ok(());
        };
        let checkpoints = self.health()?;
        let _queued = self
            .reads
            .try_acquire()
            .map_err(|_| HistoryError::Unavailable)?;
        let recovered = checkpoints
            .read_segments(stamp.clone())
            .await?
            .unwrap_or_default();
        checkpoints
            .captures
            .lock()
            .map_err(|_| HistoryError::Unavailable)?
            .recover_segments(&stamp, recovered);
        Ok(())
    }
}

impl Drop for SqliteHistoryRepository {
    fn drop(&mut self) {
        if let Some(flusher) = &self.flusher {
            flusher.abort();
        }
    }
}

impl HistoryRepository for SqliteHistoryRepository {
    fn capture<'a>(
        &'a self,
        scope: &'a HistoryScope,
        include_rows: bool,
    ) -> HistoryFuture<'a, Option<Capture>> {
        Box::pin(async move {
            if include_rows {
                let has_rows = self
                    .memory
                    .capture(scope, false)
                    .await?
                    .is_some_and(|c| c.len > 0);
                if !has_rows {
                    self.recover(scope).await?;
                }
            }
            self.memory.capture(scope, include_rows).await
        })
    }
    fn find<'a>(
        &'a self,
        scope: &'a HistoryScope,
        segmented: bool,
    ) -> HistoryFuture<'a, Option<Arc<HistorySnapshot>>> {
        Box::pin(async move { self.memory.find(scope, segmented).await })
    }
    fn capture_segments<'a>(
        &'a self,
        scope: &'a HistoryScope,
        include_rows: bool,
    ) -> HistoryFuture<'a, Option<Capture>> {
        Box::pin(async move {
            self.recover(scope).await?;
            self.memory.capture_segments(scope, include_rows).await
        })
    }
    fn load(&self, id: Uuid) -> HistoryFuture<'_, Option<Arc<HistorySnapshot>>> {
        Box::pin(async move { self.memory.load(id).await })
    }
    fn pin(&self, snapshot: HistorySnapshot) -> HistoryFuture<'_, Arc<HistorySnapshot>> {
        Box::pin(async move { self.memory.pin(snapshot).await })
    }
    fn remove(&self, id: Uuid) -> HistoryFuture<'_, ()> {
        Box::pin(async move { self.memory.remove(id).await })
    }
    fn stats(&self) -> Option<StorageStats> {
        Some(
            self.checkpoints()
                .map_or_else(StorageStats::default, |checkpoints| StorageStats {
                    healthy: checkpoints.healthy.load(Ordering::Acquire),
                    checkpoints: checkpoints.stored.load(Ordering::Relaxed),
                    wal_bytes: checkpoints.wal.load(Ordering::Relaxed),
                }),
        )
    }
}

/// What the write loop and checkpoint reads share. Its SQL runs only in jobs
/// on the shared store's worker.
struct Checkpoints {
    db: Arc<SqliteStore>,
    captures: Arc<Mutex<ScrollbackStore>>,
    /// Per configured session, the namespace its records are keyed under.
    namespaces: HashMap<String, String>,
    /// See `WRITE_DEBOUNCE`; tests that write on every pass set zero.
    debounce: Duration,
    healthy: AtomicBool,
    writer: Mutex<Writer>,
    /// How often the write loop has woken: an idle fold leaves it asleep.
    wakes: std::sync::atomic::AtomicU64,
    /// When a pass last committed a change, for the idle WAL checkpoint.
    last_write: Mutex<Instant>,
    /// Stored checkpoints and WAL bytes as of the last pass, for `stats`.
    stored: std::sync::atomic::AtomicU64,
    wal: std::sync::atomic::AtomicU64,
}

/// `(session, pane, format)`.
type ScopeKey = (String, String, String);

fn scope_key(stamp: &ProjectionStamp) -> ScopeKey {
    (
        stamp.session.clone(),
        stamp.pane.clone(),
        stamp.format.clone(),
    )
}

/// Write-loop state carried from one pass to the next.
struct Writer {
    seen: HashMap<ScopeKey, SeenProjection>,
    next_scope: Option<ScopeKey>,
}

/// Retain the last processed content independently of SQL retention. Otherwise
/// an unchanged poll could recreate a pruned/evicted row with a fresh age.
struct SeenProjection {
    stamp: ProjectionStamp,
    key: String,
    content: Option<[u8; 32]>,
    /// When this scope last changed the database.
    written: Instant,
    /// This revision was over the bounds (a row past 256 KiB, say): not
    /// stored, not tried again until the projection changes, said once.
    oversized: bool,
}

impl Checkpoints {
    async fn open(
        db: Arc<SqliteStore>,
        config: &Config,
        captures: Arc<Mutex<ScrollbackStore>>,
        debounce: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        let config = config.clone();
        let namespaces = db
            .call(move |connection| {
                let namespaces = reconcile_backends(connection, &config)?;
                prune(connection, now_seconds(), &[])?;
                Ok((namespaces, stored_count(connection)?, wal_bytes(connection)))
            })
            .await?;
        let (namespaces, stored, wal) = namespaces;
        Ok(Arc::new(Self {
            db,
            captures,
            namespaces,
            debounce,
            healthy: AtomicBool::new(true),
            writer: Mutex::new(Writer {
                seen: HashMap::new(),
                next_scope: None,
            }),
            wakes: std::sync::atomic::AtomicU64::new(0),
            last_write: Mutex::new(Instant::now()),
            stored: std::sync::atomic::AtomicU64::new(stored),
            wal: std::sync::atomic::AtomicU64::new(wal),
        }))
    }

    async fn flush_forever(self: Arc<Self>) {
        let Ok(changes) = self.captures.lock().map(|captures| captures.changes()) else {
            return;
        };
        // The fold's version as of the last pass that found nothing left to
        // do. While the fold still has exactly that version a pass would be a
        // no-op, so the loop sleeps until the fold changes or pruning is due.
        let mut settled: Option<u64> = None;
        let mut pruned = Instant::now();
        let mut failures = 0usize;
        loop {
            if settled.is_some() {
                tokio::select! {
                    () = changes.notified() => {}
                    () = tokio::time::sleep_until((pruned + PRUNE_EVERY).into()) => {}
                }
            } else {
                tokio::time::sleep(FLUSH_TICK).await;
            }
            self.wakes.fetch_add(1, Ordering::Relaxed);
            let prune_due = pruned.elapsed() >= PRUNE_EVERY;
            let snapshot = match self.captures.lock() {
                Ok(captures) => (prune_due || settled != Some(captures.version()))
                    .then(|| (captures.version(), captures.projection_stamps())),
                Err(_) => {
                    tracing::error!("captured history fold lock poisoned");
                    self.healthy.store(false, Ordering::Release);
                    return;
                }
            };
            let Some((version, stamps)) = snapshot else {
                continue;
            };
            if prune_due {
                pruned = Instant::now();
            }
            let passed = self
                .pass(now_seconds(), prune_due, stamps, self.debounce)
                .await;
            if passed.is_ok() && !self.healthy.swap(true, Ordering::AcqRel) {
                tracing::info!("captured history storage works again");
            }
            match passed {
                Ok(true) => (settled, failures) = (Some(version), 0),
                Ok(false) => (settled, failures) = (None, 0),
                Err(error) => {
                    let wait = RETRY_AFTER[failures.min(RETRY_AFTER.len() - 1)];
                    tracing::error!(%error, retry_in_s = wait.as_secs(), "captured history storage failed; realtime output and captured rows in memory remain available");
                    self.healthy.store(false, Ordering::Release);
                    failures += 1;
                    settled = None;
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// One write pass over `stamps`; `true` when it left nothing to write or delete.
    async fn pass(
        self: &Arc<Self>,
        now: i64,
        prune_due: bool,
        stamps: Vec<ProjectionStamp>,
        debounce: Duration,
    ) -> anyhow::Result<bool> {
        let this = self.clone();
        self.db
            .call(move |connection| {
                // Checkpoints are sampled and may lose their last seconds
                // (see WRITE_DEBOUNCE), so their commits skip the fsync; the
                // other domains' FULL durability is restored before any of
                // their jobs runs on this one connection.
                connection.pragma_update(None, "synchronous", "NORMAL")?;
                // FULL comes back however the job ends, a panic included:
                // the store catches it and the connection serves every domain.
                let mut durable_again = DurableAgain(connection);
                let connection = &mut *durable_again;
                // The panes the last complete listings still show: quotas evict
                // the rest first.
                let live: Vec<String> = stamps.iter().filter_map(|stamp| this.key(stamp)).collect();
                this.write_pass(connection, now, stamps, debounce, &live)
                    .and_then(|idle| {
                        if prune_due {
                            prune(connection, now, &live)?;
                        }
                        this.fold_wal(connection, prune_due)?;
                        this.stored
                            .store(stored_count(connection)?, Ordering::Relaxed);
                        this.wal.store(wal_bytes(connection), Ordering::Relaxed);
                        Ok(idle)
                    })
            })
            .await
    }

    /// Fold the WAL back into the database when it has grown past
    /// [`WAL_CHECKPOINT_BYTES`], or on a prune pass once nothing has been
    /// written for [`IDLE_CHECKPOINT`]. Runs on the store worker, never on a
    /// request.
    fn fold_wal(&self, connection: &Connection, prune_due: bool) -> anyhow::Result<()> {
        let wal = wal_bytes(connection);
        let idle = self
            .last_write
            .lock()
            .map(|at| at.elapsed() >= IDLE_CHECKPOINT)
            .unwrap_or(false);
        if wal > WAL_CHECKPOINT_BYTES || (prune_due && idle && wal > 0) {
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        }
        Ok(())
    }

    /// A pass over the fold as it is now, without pruning.
    #[cfg(test)]
    async fn flush_at(self: &Arc<Self>, now: i64) -> anyhow::Result<bool> {
        self.flush_with(now, self.debounce).await
    }

    #[cfg(test)]
    async fn flush_with(self: &Arc<Self>, now: i64, debounce: Duration) -> anyhow::Result<bool> {
        let stamps = self
            .captures
            .lock()
            .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
            .projection_stamps();
        self.pass(now, false, stamps, debounce).await
    }

    async fn read_segments(
        self: &Arc<Self>,
        stamp: ProjectionStamp,
    ) -> Result<Option<Vec<Segment>>, HistoryError> {
        let this = self.clone();
        self.db
            .call(move |connection| this.read_rows(connection, &stamp))
            .await
            .map_err(|error| {
                tracing::error!(%error, "captured history read failed");
                self.healthy.store(false, Ordering::Release);
                HistoryError::Unavailable
            })
    }

    #[cfg(test)]
    async fn read(
        self: &Arc<Self>,
        stamp: ProjectionStamp,
    ) -> Result<Option<(Vec<String>, Instant)>, HistoryError> {
        Ok(self.read_segments(stamp).await?.map(|segments| {
            let remaining = segments
                .iter()
                .map(|s| {
                    s.changed_at_ms
                        .saturating_add(RETENTION_MS)
                        .saturating_sub(super::history_segments::now_ms())
                })
                .min()
                .unwrap_or(0)
                .max(0) as u64;
            (
                segments
                    .iter()
                    .flat_map(|s| &s.rows)
                    .map(|r| r.to_string())
                    .collect(),
                Instant::now() + Duration::from_millis(remaining),
            )
        }))
    }

    /// `None` for a pane whose process identity is unverified: such a
    /// projection could never be recovered, so it is never written.
    fn key(&self, stamp: &ProjectionStamp) -> Option<String> {
        let namespace = self.namespaces.get(&stamp.session)?;
        let identity = stamp.native_identity.as_ref()?;
        Some(
            serde_json::json!([
                namespace,
                identity,
                stamp.pane,
                "recent_unwrapped",
                stamp.format
            ])
            .to_string(),
        )
    }

    fn read_rows(
        &self,
        connection: &Connection,
        stamp: &ProjectionStamp,
    ) -> anyhow::Result<Option<Vec<Segment>>> {
        let Some(key) = self.key(stamp) else {
            return Ok(None);
        };
        let policy: Option<String> = connection
            .query_row(
                "SELECT policy FROM history_captures WHERE identity=?1",
                [&key],
                |row| {
                    let policy = row.get_ref(0)?.as_str()?;
                    if policy.len() > 4096 {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    Ok(policy.to_owned())
                },
            )
            .optional()?;
        let Some(policy) = policy else {
            return Ok(None);
        };
        // Dimensions and editor/capture policy delimit sampling, not native
        // incarnation. Workspace/tab/terminal identity still must match.
        let identity = |p: &str| -> anyhow::Result<Value> {
            let value: Value = serde_json::from_str(p)?;
            let policy = value
                .as_array()
                .filter(|p| p.len() == 3)
                .ok_or_else(|| anyhow::anyhow!("invalid stored policy"))?;
            let identity = policy[0]
                .as_array()
                .filter(|i| i.len() == 3)
                .ok_or_else(|| anyhow::anyhow!("invalid stored identity"))?;
            anyhow::ensure!(
                identity.iter().all(|i| i.is_null() || i.is_string()),
                "invalid stored identity"
            );
            Ok(policy[0].clone())
        };
        if identity(&policy)? != identity(&stamp.policy)? {
            return Ok(None);
        }
        let cutoff = super::history_segments::now_ms() - RETENTION_MS;
        let (count, row_bytes, metadata_bytes): (i64, i64, i64) = connection.query_row(
            "SELECT count(*),coalesce(sum(length(rows)),0),coalesce(sum(length(CAST(position AS BLOB))),0) FROM history_segments WHERE identity=?1 AND changed_ms>?2",
            params![key, cutoff], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))
        )?;
        anyhow::ensure!(
            count <= MAX_SEGMENTS as i64
                && row_bytes <= MAX_ENCODED_BYTES as i64
                && metadata_bytes <= (MAX_SEGMENTS * 512) as i64,
            "stored shape exceeds bounds"
        );
        let encoded: Vec<(String, String, Vec<u8>, i64)> = connection.prepare(
            "SELECT segment_id,position,rows,changed_ms FROM history_segments WHERE identity=?1 AND changed_ms>?2 ORDER BY json_extract(position,'$.started_at_ms'),rowid"
        )?.query_map(params![key, cutoff], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?.collect::<Result<_,_>>()?;
        anyhow::ensure!(
            encoded.len() <= MAX_SEGMENTS,
            "stored segment count exceeds bounds"
        );
        let mut segments = Vec::new();
        for (id, position, encoded, changed_at_ms) in encoded {
            let mut position: SegmentPosition = serde_json::from_str(&position)?;
            position.segment_id = Uuid::parse_str(&id)?.to_string();
            let rows = decode_rows(&encoded)?
                .into_iter()
                .filter(|row| super::exit_marker::strip_exit_markers(row).as_ref() == row)
                .map(super::scrollback::Row::from)
                .collect::<Vec<_>>();
            position.len = rows.len();
            anyhow::ensure!(
                position.base.checked_add(rows.len() as u64).is_some(),
                "invalid stored row positions"
            );
            if !rows.is_empty() {
                segments.push(Segment {
                    position,
                    changed_at_ms,
                    rows,
                });
            }
        }
        anyhow::ensure!(
            segments.iter().map(|s| s.rows.len()).sum::<usize>() <= MAX_PANE_LINES
                && segments
                    .iter()
                    .flat_map(|s| &s.rows)
                    .map(|r| r.len())
                    .sum::<usize>()
                    <= MAX_PANE_BYTES,
            "stored shape exceeds bounds"
        );
        Ok(Some(segments))
    }

    /// One pass: forget one vanished or unverified scope, or write one changed
    /// projection. Returns `true` when there was nothing left to do.
    fn write_pass(
        &self,
        connection: &mut Connection,
        now: i64,
        mut stamps: Vec<ProjectionStamp>,
        debounce: Duration,
        open_keys: &[String],
    ) -> anyhow::Result<bool> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("history writer lock poisoned"))?;
        let live: HashSet<_> = stamps.iter().map(scope_key).collect();
        let gone = writer.seen.keys().find(|key| !live.contains(*key)).cloned();
        if let Some(scope) = gone {
            if let Some(seen) = writer.seen.get(&scope) {
                connection.execute(
                    "DELETE FROM history_captures WHERE identity=?1",
                    [&seen.key],
                )?;
            }
            writer.seen.remove(&scope);
            return Ok(false);
        }
        stamps.sort_by(|a, b| {
            (&a.session, &a.pane, &a.format).cmp(&(&b.session, &b.pane, &b.format))
        });
        if let Some(last) = &writer.next_scope {
            let start = stamps.partition_point(|s| {
                (&s.session, &s.pane, &s.format) <= (&last.0, &last.1, &last.2)
            });
            stamps.rotate_left(start);
        }
        // One bounded projection per pass. Dirty revisions remain in the fold,
        // so overload coalesces to the latest state.
        let mut idle = true;
        for stamp in stamps {
            let scope_key = scope_key(&stamp);
            let seen = writer.seen.get(&scope_key);
            if seen.is_some_and(|seen| seen.stamp == stamp) {
                continue;
            }
            let Some(key) = self.key(&stamp) else {
                // Verified -> unknown: what the verified process wrote goes.
                if let Some(seen) = writer.seen.get(&scope_key) {
                    connection.execute(
                        "DELETE FROM history_captures WHERE identity=?1",
                        [&seen.key],
                    )?;
                    writer.seen.remove(&scope_key);
                    return Ok(false);
                }
                continue;
            };
            anyhow::ensure!(
                key.len() <= 8192 && stamp.policy.len() <= 4096,
                "capture metadata exceeds bounds"
            );
            // Resolve recovery on the same store worker BEFORE any replacement,
            // even when output has already produced a new historical prefix.
            // Read/decoder errors leave recovery pending and the old SQL intact.
            let pending = self
                .captures
                .lock()
                .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                .recovery_pending(&stamp);
            if pending {
                let recovered = self.read_rows(connection, &stamp)?.unwrap_or_default();
                if !self
                    .captures
                    .lock()
                    .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                    .recover_segments(&stamp, recovered)
                {
                    continue;
                }
            }
            // A reset or a new identity is written at once; so is a projection
            // that emptied, whose row is deleted only when `stamp.reset` is set
            // (otherwise the stored row stays).
            let urgent = seen.is_none_or(|seen| seen.key != key || seen.stamp.fence != stamp.fence);
            let debounced = !urgent && seen.is_some_and(|seen| seen.written.elapsed() < debounce);
            let capture = {
                let store = self
                    .captures
                    .lock()
                    .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?;
                if store.projection_stamp(&stamp.scope()).as_ref() != Some(&stamp) {
                    continue;
                }
                if debounced
                    && store
                        .live_captured_history(&stamp.scope(), false)
                        .ok()
                        .flatten()
                        .is_some_and(|capture| capture.len > 0)
                {
                    idle = false;
                    continue;
                }
                // Shared rows only: the copy and the encoding happen below,
                // without the lock every read and frame waits on.
                store.segment_projection(&stamp.scope())
            };
            let encoded = capture.and_then(|segments| {
                segments
                    .into_iter()
                    .map(|s| {
                        let rows = encode_rows(&s.rows);
                        Ok((
                            s.position.segment_id.to_string(),
                            serde_json::to_string(&s.position)
                                .map_err(|_| HistoryError::Unavailable)?,
                            rows,
                            s.changed_at_ms,
                        ))
                    })
                    .collect::<Result<Vec<_>, HistoryError>>()
            });
            // Over the bounds: this pane's outcome, not a storage failure.
            // The others are still written and the store stays healthy.
            let Some(encoded) = encoded.ok().filter(|encoded| {
                encoded
                    .iter()
                    .map(|(_, position, rows, _)| rows.len() + position.len())
                    .sum::<usize>()
                    <= MAX_ENCODED_BYTES + MAX_SEGMENTS * 512
            }) else {
                if !seen.is_some_and(|seen| seen.oversized && seen.key == key) {
                    tracing::warn!(
                        pane = %stamp.pane,
                        format = %stamp.format,
                        "captured history: a projection is over the checkpoint bounds; not stored until it changes"
                    );
                }
                connection.execute("DELETE FROM history_captures WHERE identity=?1", [&key])?;
                writer.next_scope = Some(scope_key.clone());
                writer.seen.insert(
                    scope_key,
                    SeenProjection {
                        stamp,
                        key,
                        content: None,
                        written: Instant::now(),
                        oversized: true,
                    },
                );
                continue;
            };
            let content: Option<[u8; 32]> = (!encoded.is_empty()).then(|| {
                let mut hash = Sha256::new();
                for (id, position, rows, changed) in &encoded {
                    hash.update(id);
                    hash.update(position);
                    hash.update(rows);
                    hash.update(changed.to_le_bytes());
                }
                hash.finalize().into()
            });
            if encoded.is_empty()
                && seen.is_none()
                && (!stamp.reset
                    || self
                        .captures
                        .lock()
                        .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                        .absence_acknowledged(&stamp))
            {
                continue;
            }
            let previous = writer.seen.get(&scope_key);
            let unchanged = previous.is_some_and(|seen| {
                seen.key == key
                    && seen.stamp.fence == stamp.fence
                    && seen.stamp.policy == stamp.policy
                    && seen.content == content
            });
            let mut written = previous.map(|seen| seen.written);
            if !unchanged {
                let transaction = connection.transaction()?;
                if let Some(seen) = previous {
                    if seen.key != key || seen.stamp.fence != stamp.fence {
                        transaction.execute(
                            "DELETE FROM history_captures WHERE identity=?1",
                            [&seen.key],
                        )?;
                    }
                }
                if !encoded.is_empty() {
                    let bytes = encoded
                        .iter()
                        .map(|(id, position, rows, _)| rows.len() + position.len() + id.len() + 16)
                        .sum::<usize>() as i64
                        + key.len() as i64
                        + stamp.policy.len() as i64
                        + 64;
                    let updated = encoded
                        .iter()
                        .map(|(_, _, _, changed)| *changed / 1000)
                        .max()
                        .unwrap_or(now);
                    transaction.execute("INSERT INTO history_captures VALUES (?1,?2,x'',?3,?4) ON CONFLICT(identity) DO UPDATE SET policy=excluded.policy, bytes=excluded.bytes, updated=excluded.updated", params![key, stamp.policy, bytes, updated])?;
                    transaction
                        .execute("DELETE FROM history_segments WHERE identity=?1", [&key])?;
                    for (id, position, rows, changed) in encoded {
                        transaction.execute(
                            "INSERT INTO history_segments VALUES (?1,?2,?3,?4,?5,?6)",
                            params![
                                key,
                                id,
                                position,
                                rows,
                                (rows.len() + position.len() + id.len() + 16) as i64,
                                changed
                            ],
                        )?;
                    }
                    enforce_quotas(&transaction, open_keys)?;
                } else if stamp.reset || !pending {
                    transaction
                        .execute("DELETE FROM history_captures WHERE identity=?1", [&key])?;
                }
                transaction.commit()?;
                written = Some(Instant::now());
                if let Ok(mut at) = self.last_write.lock() {
                    *at = Instant::now();
                }
            }
            let still_current = self
                .captures
                .lock()
                .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                .projection_stamp(&stamp.scope())
                .as_ref()
                == Some(&stamp);
            if !still_current {
                // A fresh frame during the write leaves the newer revision dirty,
                // but must not delete recovered prefixes from the committed set.
                // Only an observed identity reset authorizes a tombstone.
                let invalid_identity = self
                    .captures
                    .lock()
                    .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                    .projection_stamp(&stamp.scope())
                    .is_none_or(|current| {
                        current.native_identity != stamp.native_identity
                            || current.reset != stamp.reset
                    });
                if invalid_identity {
                    connection.execute("DELETE FROM history_captures WHERE identity=?1", [&key])?;
                }
                continue;
            }
            if content.is_none() {
                self.captures
                    .lock()
                    .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                    .acknowledge_absence(&stamp);
                writer.seen.remove(&scope_key);
                idle = false;
                break;
            }
            // A concurrent reset leaves a different stamp dirty; an old ack
            // cannot make that new revision clean or authorize recovery.
            writer.next_scope = Some(scope_key.clone());
            writer.seen.insert(
                scope_key,
                SeenProjection {
                    stamp,
                    key,
                    content,
                    written: written.unwrap_or_else(Instant::now),
                    oversized: false,
                },
            );
            idle = false;
            break;
        }
        Ok(idle)
    }
}

/// Reconcile the backend registry with the startup configuration and return
/// each session's namespace. A changed endpoint or install rotates the
/// incarnation; records of removed sessions become unreachable and age out.
fn reconcile_backends(
    connection: &mut Connection,
    config: &Config,
) -> anyhow::Result<HashMap<String, String>> {
    let transaction = connection.transaction()?;
    let mut namespaces = HashMap::new();
    for session in &config.sessions {
        // Canonicalize the parent even while a configured backend is offline.
        let endpoint = Path::new(&session.socket_path);
        let canonical = endpoint
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .map(|p| p.join(endpoint.file_name().unwrap_or_default()))
            .unwrap_or_else(|| endpoint.to_path_buf());
        let endpoint =
            serde_json::json!([config.server_id, session.backend.as_str(), canonical]).to_string();
        let previous: Option<(String, String)> = transaction
            .query_row(
                "SELECT endpoint, incarnation FROM history_backends WHERE session=?1",
                [&session.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let incarnation = previous
            .filter(|(old, _)| *old == endpoint)
            .map(|(_, id)| id)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        transaction.execute("INSERT INTO history_backends VALUES (?1,?2,?3) ON CONFLICT(session) DO UPDATE SET endpoint=excluded.endpoint, incarnation=excluded.incarnation", params![session.id, endpoint, incarnation])?;
        namespaces.insert(
            session.id.clone(),
            serde_json::json!([endpoint, incarnation, session.id]).to_string(),
        );
    }
    let stored: Vec<String> = transaction
        .prepare("SELECT session FROM history_backends")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for session in stored {
        if !namespaces.contains_key(&session) {
            transaction.execute("DELETE FROM history_backends WHERE session=?1", [session])?;
        }
    }
    transaction.commit()?;
    Ok(namespaces)
}

/// Age out records unchanged for the retention period, then apply the logical
/// quotas.
fn prune(connection: &mut Connection, now: i64, live: &[String]) -> anyhow::Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM history_captures WHERE updated < ?1",
        [now - RETENTION_SECONDS],
    )?;
    transaction.execute(
        "DELETE FROM history_segments WHERE changed_ms <= ?1",
        [(now - RETENTION_SECONDS) * 1000],
    )?;
    transaction.execute("UPDATE history_captures SET bytes=(SELECT coalesce(sum(bytes),0) FROM history_segments WHERE history_segments.identity=history_captures.identity)+length(CAST(identity AS BLOB))+length(CAST(policy AS BLOB))+64 WHERE rows=x''", [])?;
    transaction.execute("DELETE FROM history_captures WHERE rows=x'' AND NOT EXISTS(SELECT 1 FROM history_segments WHERE history_segments.identity=history_captures.identity)", [])?;
    enforce_quotas(&transaction, live)?;
    transaction.commit()?;
    Ok(())
}

/// Keep the newest actual updates that fit both the record and the payload
/// quota, the `live` panes' (those the last complete listings still show)
/// ranked before every other; evict the rest over the covering index. A live
/// pane's checkpoint evicted for quota is logged: that is history a user may
/// still page through.
fn enforce_quotas(transaction: &Transaction<'_>, live: &[String]) -> anyhow::Result<()> {
    let live_json = serde_json::to_string(live)?;
    let evicted: Vec<(String, bool)> = transaction
        .prepare(
            "SELECT identity, live FROM (SELECT identity, live, row_number() OVER w n, sum(bytes) OVER w t FROM (SELECT identity, bytes, updated, identity IN (SELECT value FROM json_each(?3)) live FROM history_captures) WINDOW w AS (ORDER BY live DESC, updated DESC, identity DESC)) WHERE n > ?1 OR t > ?2",
        )?
        .query_map(params![MAX_RECORDS, MAX_PAYLOAD_BYTES, live_json], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    for (identity, _) in &evicted {
        transaction.execute("DELETE FROM history_captures WHERE identity=?1", [identity])?;
    }
    let live_evicted = evicted.iter().filter(|(_, live)| *live).count();
    if live_evicted > 0 {
        tracing::warn!(
            panes = live_evicted,
            "captured history quota evicted the checkpoint of a pane that is still open"
        );
    }
    Ok(())
}

/// The shared connection, set back to `synchronous=FULL` when dropped.
struct DurableAgain<'a>(&'a mut Connection);

impl std::ops::Deref for DurableAgain<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.0
    }
}

impl std::ops::DerefMut for DurableAgain<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.0
    }
}

impl Drop for DurableAgain<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.pragma_update(None, "synchronous", "FULL") {
            tracing::error!(%error, "the shared store could not go back to synchronous=FULL");
        }
    }
}

/// How many checkpoints are stored, over the covering index.
fn stored_count(connection: &Connection) -> anyhow::Result<u64> {
    let count: i64 = connection.query_row("SELECT count(*) FROM history_captures", [], |row| {
        row.get(0)
    })?;
    Ok(count.max(0) as u64)
}

/// The size of the store's WAL file; 0 when it has none.
fn wal_bytes(connection: &Connection) -> u64 {
    connection
        .path()
        .filter(|path| !path.is_empty())
        .and_then(|path| std::fs::metadata(format!("{path}-wal")).ok())
        .map_or(0, |wal| wal.len())
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn encode_rows<R: AsRef<str>>(rows: &[R]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(rows.iter().map(|row| row.as_ref().len() + 4).sum());
    for row in rows {
        let row = row.as_ref();
        encoded.extend_from_slice(&(row.len() as u32).to_le_bytes());
        encoded.extend_from_slice(row.as_bytes());
    }
    encoded
}

fn decode_rows(mut encoded: &[u8]) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        encoded.len() <= MAX_ENCODED_BYTES,
        "stored projection exceeds bounds"
    );
    let mut rows = Vec::new();
    while !encoded.is_empty() {
        anyhow::ensure!(
            encoded.len() >= 4 && rows.len() < MAX_PANE_LINES,
            "invalid stored projection"
        );
        let length = u32::from_le_bytes(encoded[..4].try_into()?) as usize;
        encoded = &encoded[4..];
        anyhow::ensure!(
            length < super::history::MAX_PAGE_BYTES && length <= encoded.len(),
            "invalid stored row"
        );
        rows.push(std::str::from_utf8(&encoded[..length])?.to_owned());
        encoded = &encoded[length..];
    }
    anyhow::ensure!(
        rows.len() <= MAX_PANE_LINES
            && rows
                .iter()
                .all(|r| r.len() < super::history::MAX_PAGE_BYTES),
        "invalid stored projection"
    );
    anyhow::ensure!(
        rows.iter().map(String::len).sum::<usize>() <= MAX_PANE_BYTES,
        "stored projection exceeds row text bounds"
    );
    Ok(rows)
}

#[cfg(test)]
mod tests;
