//! Optional checkpoint storage. Only the dedicated worker owns SQLite. The live
//! fold coalesces revisions; the worker copies at most one bounded projection
//! under its lock, then performs transactions without that lock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::history::{
    Capture, HistoryError, HistoryFuture, HistoryRepository, HistoryScope, HistorySnapshot,
};
use super::history_memory::MemoryHistoryRepository;
use super::scrollback::{ProjectionStamp, ScrollbackStore, MAX_PANE_BYTES, MAX_PANE_LINES};
use crate::platform::config::{Config, HistoryStorage};

const MAX_RECORDS: i64 = 48;
const MAX_PAYLOAD_BYTES: i64 = 24 * 1024 * 1024;
// The pane cap measures row text, just like the live fold and snapshot cache.
// Length prefixes have their own bounded overhead, counted in disk quotas.
const MAX_ENCODED_BYTES: usize = MAX_PANE_BYTES + 4 * MAX_PANE_LINES;
const MAX_MAIN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;
const RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
const WORKER_TICK: Duration = Duration::from_millis(100);

type Reply<T> = oneshot::Sender<Result<T, HistoryError>>;
enum Command {
    Read(ProjectionStamp, Reply<Option<(Vec<String>, Instant)>>),
    Health(Reply<()>),
}

pub(crate) struct SqliteHistoryRepository {
    memory: MemoryHistoryRepository,
    captures: Arc<Mutex<ScrollbackStore>>,
    commands: Option<mpsc::SyncSender<Command>>,
    #[cfg(test)]
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Memory selection does not even resolve the state/history path.
pub(crate) async fn repository(
    config: &Config,
    captures: Arc<Mutex<ScrollbackStore>>,
) -> Arc<dyn HistoryRepository> {
    match config.history.storage {
        HistoryStorage::Memory => Arc::new(MemoryHistoryRepository::new(captures)),
        HistoryStorage::Sqlite => {
            let path = crate::state_dir().map(|path| path.join("history"));
            Arc::new(SqliteHistoryRepository::open(path, config.clone(), captures).await)
        }
    }
}

impl SqliteHistoryRepository {
    pub(super) async fn open(
        directory: anyhow::Result<PathBuf>,
        config: Config,
        captures: Arc<Mutex<ScrollbackStore>>,
    ) -> Self {
        let memory = MemoryHistoryRepository::new(captures.clone());
        let (commands, receive) = mpsc::sync_channel(16);
        let (ready, initialization) = oneshot::channel();
        let fold = captures.clone();
        let started = std::thread::Builder::new().name("captured-history".into()).spawn(move || {
            let opened = directory.and_then(|path| Worker::open(path, &config, fold));
            match opened {
                Ok(mut worker) => {
                    let _ = ready.send(Ok(()));
                    worker.run(receive);
                }
                Err(error) => {
                    tracing::error!(%error, "captured history initialization failed; realtime output remains available");
                    let _ = ready.send(Err(HistoryError::Unavailable));
                }
            }
        });
        let commands = if started.is_ok() && initialization.await == Ok(Ok(())) {
            Some(commands)
        } else {
            None
        };
        Self {
            memory,
            captures,
            commands,
            #[cfg(test)]
            worker: started.ok(),
        }
    }

    async fn health(&self) -> Result<(), HistoryError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Health(reply))?;
        receive.await.map_err(|_| HistoryError::Unavailable)?
    }

    fn send(&self, command: Command) -> Result<(), HistoryError> {
        self.commands
            .as_ref()
            .ok_or(HistoryError::Unavailable)?
            .try_send(command)
            .map_err(|_| HistoryError::Unavailable)
    }

    #[cfg(test)]
    pub(super) async fn close(mut self) {
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            tokio::task::spawn_blocking(move || worker.join().unwrap())
                .await
                .unwrap();
        }
    }

    async fn recover(&self, scope: &HistoryScope) -> Result<(), HistoryError> {
        let stamp = self
            .captures
            .lock()
            .map_err(|_| HistoryError::Unavailable)?
            .projection_stamp(scope);
        let Some(stamp) = stamp.filter(|stamp| stamp.native_identity.is_some() && !stamp.reset)
        else {
            return Ok(());
        };
        let (reply, receive) = oneshot::channel();
        self.send(Command::Read(stamp.clone(), reply))?;
        let rows = receive.await.map_err(|_| HistoryError::Unavailable)??;
        if let Some((rows, expires)) = rows {
            self.captures
                .lock()
                .map_err(|_| HistoryError::Unavailable)?
                .recover_checkpoint(&stamp, rows, expires);
        }
        Ok(())
    }
}

impl HistoryRepository for SqliteHistoryRepository {
    fn capture<'a>(
        &'a self,
        scope: &'a HistoryScope,
        include_rows: bool,
    ) -> HistoryFuture<'a, Option<Capture>> {
        Box::pin(async move {
            self.health().await?;
            if include_rows {
                let has_rows = self
                    .memory
                    .capture(scope, true)
                    .await?
                    .is_some_and(|c| !c.rows.is_empty());
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
    ) -> HistoryFuture<'a, Option<Arc<HistorySnapshot>>> {
        Box::pin(async move {
            self.health().await?;
            self.memory.find(scope).await
        })
    }
    fn load(&self, id: Uuid) -> HistoryFuture<'_, Option<Arc<HistorySnapshot>>> {
        Box::pin(async move {
            self.health().await?;
            self.memory.load(id).await
        })
    }
    fn pin(&self, snapshot: HistorySnapshot) -> HistoryFuture<'_, Arc<HistorySnapshot>> {
        Box::pin(async move {
            self.health().await?;
            self.memory.pin(snapshot).await
        })
    }
    fn remove(&self, id: Uuid) -> HistoryFuture<'_, ()> {
        Box::pin(async move {
            self.health().await?;
            self.memory.remove(id).await
        })
    }
}

struct Worker {
    connection: Connection,
    directory: PathBuf,
    namespaces: HashMap<String, String>,
    run: Uuid,
    captures: Arc<Mutex<ScrollbackStore>>,
    seen: HashMap<(String, String, String), SeenProjection>,
    healthy: bool,
    retention: Instant,
    next_scope: Option<(String, String, String)>,
}

/// Retain the last processed content independently of SQL retention. Otherwise
/// an unchanged poll could recreate a pruned/evicted row with a fresh age.
struct SeenProjection {
    stamp: ProjectionStamp,
    key: String,
    content: Option<[u8; 32]>,
}

impl Worker {
    fn open(
        directory: PathBuf,
        config: &Config,
        captures: Arc<Mutex<ScrollbackStore>>,
    ) -> anyhow::Result<Self> {
        secure_directory(&directory)?;
        let path = directory.join("captures.sqlite3");
        secure_file(&path, true)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            secure_file(&directory.join(format!("captures.sqlite3{suffix}")), false)?;
        }
        anyhow::ensure!(
            std::fs::metadata(&path)?.len() <= MAX_MAIN_BYTES,
            "captured history main file exceeds physical quota"
        );
        if let Ok(wal) = std::fs::metadata(directory.join("captures.sqlite3-wal")) {
            anyhow::ensure!(
                wal.len() <= MAX_WAL_BYTES,
                "captured history WAL exceeds physical quota"
            );
        }
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(Duration::from_millis(100))?;
        migrate(&mut connection)?;
        let pages: u64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
        let page_size: u64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        anyhow::ensure!(
            pages * page_size <= MAX_MAIN_BYTES,
            "captured history exceeds physical quota"
        );
        connection.pragma_update(None, "max_page_count", MAX_MAIN_BYTES / page_size)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "wal_autocheckpoint", 256)?;
        connection.pragma_update(None, "journal_size_limit", MAX_WAL_BYTES / 2)?;
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
                serde_json::json!([config.server_id, session.backend.as_str(), canonical])
                    .to_string();
            let previous: Option<(String, String)> = transaction
                .query_row(
                    "SELECT endpoint, incarnation FROM backends WHERE session=?1",
                    [&session.id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let incarnation = previous
                .filter(|(old, _)| *old == endpoint)
                .map(|(_, id)| id)
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            transaction.execute("INSERT INTO backends VALUES (?1,?2,?3) ON CONFLICT(session) DO UPDATE SET endpoint=excluded.endpoint, incarnation=excluded.incarnation", params![session.id, endpoint, incarnation])?;
            namespaces.insert(
                session.id.clone(),
                serde_json::json!([endpoint, incarnation, session.id]).to_string(),
            );
        }
        let mut stored = Vec::new();
        {
            let mut query = transaction.prepare("SELECT session FROM backends")?;
            for session in query.query_map([], |row| row.get::<_, String>(0))? {
                stored.push(session?);
            }
        }
        for session in stored {
            if !namespaces.contains_key(&session) {
                transaction.execute("DELETE FROM backends WHERE session=?1", [session])?;
            }
        }
        transaction.commit()?;
        let mut worker = Self {
            connection,
            directory,
            namespaces,
            run: Uuid::new_v4(),
            captures,
            seen: HashMap::new(),
            healthy: true,
            retention: Instant::now(),
            next_scope: None,
        };
        worker.prune()?;
        worker.checkpoint()?;
        Ok(worker)
    }

    fn key(&self, stamp: &ProjectionStamp) -> Option<String> {
        let namespace = self.namespaces.get(&stamp.session)?;
        Some(
            serde_json::json!([
                namespace,
                stamp
                    .native_identity
                    .clone()
                    .unwrap_or_else(|| format!("unknown:{}", self.run)),
                stamp.pane,
                "recent_unwrapped",
                stamp.format
            ])
            .to_string(),
        )
    }

    fn run(&mut self, receive: mpsc::Receiver<Command>) {
        loop {
            if self.healthy {
                if let Err(error) = self.flush() {
                    tracing::error!(%error, "captured history storage failed; realtime output remains available");
                    self.healthy = false;
                }
            }
            match receive.recv_timeout(WORKER_TICK) {
                Ok(Command::Health(reply)) => {
                    let _ = reply.send(if self.healthy {
                        Ok(())
                    } else {
                        Err(HistoryError::Unavailable)
                    });
                }
                Ok(Command::Read(stamp, reply)) => {
                    let result = if self.healthy {
                        self.read(&stamp).map_err(|error| {
                            tracing::error!(%error, "captured history read failed");
                            self.healthy = false;
                            HistoryError::Unavailable
                        })
                    } else {
                        Err(HistoryError::Unavailable)
                    };
                    let _ = reply.send(result);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if self.healthy {
                        let _ = self.flush();
                        let _ = self.checkpoint();
                    }
                    break;
                }
            }
        }
    }

    fn read(&self, stamp: &ProjectionStamp) -> anyhow::Result<Option<(Vec<String>, Instant)>> {
        let Some(key) = self.key(stamp) else {
            return Ok(None);
        };
        let encoded: Option<(Vec<u8>, i64)> = self
            .connection
            .query_row(
                "SELECT rows,updated FROM captures WHERE identity=?1 AND policy=?2 AND updated>=?3",
                params![key, stamp.policy, now_seconds() - RETENTION_SECONDS],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        encoded
            .map(|(encoded, updated)| {
                Ok((
                    decode_rows(&encoded)?,
                    Instant::now()
                        + Duration::from_secs(
                            updated
                                .saturating_add(RETENTION_SECONDS)
                                .saturating_sub(now_seconds())
                                .clamp(0, RETENTION_SECONDS) as u64,
                        ),
                ))
            })
            .transpose()
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.flush_at(now_seconds())
    }

    fn flush_at(&mut self, now: i64) -> anyhow::Result<()> {
        let mut stamps = self
            .captures
            .lock()
            .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
            .projection_stamps();
        let live: std::collections::HashSet<_> = stamps
            .iter()
            .map(|s| (s.session.clone(), s.pane.clone(), s.format.clone()))
            .collect();
        let gone: Vec<_> = self
            .seen
            .keys()
            .filter(|key| !live.contains(*key))
            .cloned()
            .collect();
        if let Some(scope) = gone.into_iter().next() {
            if let Some(seen) = self.seen.get(&scope) {
                self.checkpoint()?;
                self.connection
                    .execute("DELETE FROM captures WHERE identity=?1", [&seen.key])?;
                self.checkpoint()?;
                self.seen.remove(&scope);
            }
            return Ok(());
        }
        stamps.sort_by(|a, b| {
            (&a.session, &a.pane, &a.format).cmp(&(&b.session, &b.pane, &b.format))
        });
        if let Some(last) = &self.next_scope {
            let start = stamps.partition_point(|s| {
                (&s.session, &s.pane, &s.format) <= (&last.0, &last.1, &last.2)
            });
            stamps.rotate_left(start);
        }
        // One transaction and one bounded projection per pass. Dirty
        // revisions remain in the fold, so overload coalesces to the latest state.
        for stamp in stamps {
            let scope_key = (
                stamp.session.clone(),
                stamp.pane.clone(),
                stamp.format.clone(),
            );
            if self
                .seen
                .get(&scope_key)
                .is_some_and(|seen| seen.stamp == stamp)
            {
                continue;
            }
            let Some(key) = self.key(&stamp) else {
                continue;
            };
            anyhow::ensure!(
                key.len() <= 8192 && stamp.policy.len() <= 4096,
                "capture metadata exceeds bounds"
            );
            let capture = {
                let store = self
                    .captures
                    .lock()
                    .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?;
                if store.projection_stamp(&stamp.scope()).as_ref() != Some(&stamp) {
                    continue;
                }
                store
                    .live_captured_history(&stamp.scope(), true)
                    .map_err(|_| anyhow::anyhow!("capture projection exceeds bounds"))?
            };
            let encoded = capture
                .filter(|capture| !capture.rows.is_empty())
                .map(|capture| encode_rows(&capture.rows));
            if let Some(encoded) = &encoded {
                anyhow::ensure!(
                    encoded.len() <= MAX_ENCODED_BYTES,
                    "encoded projection exceeds bounds"
                );
            }
            let content: Option<[u8; 32]> = encoded
                .as_ref()
                .map(|encoded| Sha256::digest(encoded).into());
            let unchanged = self.seen.get(&scope_key).is_some_and(|seen| {
                seen.key == key
                    && seen.stamp.fence == stamp.fence
                    && seen.stamp.policy == stamp.policy
                    && seen.content == content
            });
            self.checkpoint()?;
            let transaction = self.connection.transaction()?;
            if !unchanged {
                if let Some(seen) = self.seen.get(&scope_key) {
                    if seen.key != key || seen.stamp.fence != stamp.fence {
                        transaction
                            .execute("DELETE FROM captures WHERE identity=?1", [&seen.key])?;
                    }
                }
                if let Some(encoded) = encoded {
                    transaction.execute("INSERT INTO captures VALUES (?1,?2,?3,?4,?5) ON CONFLICT(identity) DO UPDATE SET policy=excluded.policy, rows=excluded.rows, bytes=excluded.bytes, updated=excluded.updated WHERE captures.rows != excluded.rows OR captures.policy != excluded.policy", params![key, stamp.policy, encoded, encoded.len(), now])?;
                } else if stamp.reset {
                    transaction.execute("DELETE FROM captures WHERE identity=?1", [&key])?;
                }
            }
            prune_transaction(&transaction)?;
            transaction.commit()?;
            let still_current = self
                .captures
                .lock()
                .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
                .projection_stamp(&stamp.scope())
                .as_ref()
                == Some(&stamp);
            if !still_current {
                // The only writer serializes this compensating tombstone before
                // another projection. Never publish an ack for a stale revision.
                if !unchanged {
                    self.connection
                        .execute("DELETE FROM captures WHERE identity=?1", [&key])?;
                }
                self.checkpoint()?;
                continue;
            }
            self.checkpoint()?;
            // A concurrent reset leaves a different stamp dirty; an old ack
            // cannot make that new revision clean or authorize recovery.
            self.next_scope = Some(scope_key.clone());
            self.seen.insert(
                scope_key,
                SeenProjection {
                    stamp,
                    key,
                    content,
                },
            );
            break;
        }
        if self.retention.elapsed() >= Duration::from_secs(60) {
            self.prune_at(now)?;
            self.checkpoint()?;
            self.retention = Instant::now();
        }
        Ok(())
    }

    fn prune(&mut self) -> anyhow::Result<()> {
        self.prune_at(now_seconds())
    }

    fn prune_at(&mut self, now: i64) -> anyhow::Result<()> {
        // Delete one bounded payload per transaction. Expiring 24 MiB at once
        // would otherwise make a WAL larger than its physical ceiling.
        loop {
            let (count, bytes): (i64, i64) = self.connection.query_row(
                "SELECT count(*), coalesce(sum(bytes),0) FROM captures",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let oldest: Option<(String, i64, i64)> = self.connection.query_row("SELECT identity, updated, length(rows) FROM captures ORDER BY updated,identity LIMIT 1", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
            let Some((key, updated, length)) = oldest else {
                break;
            };
            anyhow::ensure!(
                length <= MAX_ENCODED_BYTES as i64,
                "stored payload exceeds transaction quota"
            );
            if count <= MAX_RECORDS
                && bytes <= MAX_PAYLOAD_BYTES
                && updated >= now - RETENTION_SECONDS
            {
                break;
            }
            self.checkpoint()?;
            let transaction = self.connection.transaction()?;
            transaction.execute("DELETE FROM captures WHERE identity=?1", [key])?;
            transaction.commit()?;
            self.checkpoint()?;
        }
        Ok(())
    }

    fn checkpoint(&self) -> anyhow::Result<()> {
        let path = self.directory.join("captures.sqlite3");
        for suffix in ["", "-wal", "-shm"] {
            secure_file(
                &self.directory.join(format!("captures.sqlite3{suffix}")),
                false,
            )?;
        }
        let main = std::fs::metadata(&path)?.len();
        let wal = std::fs::metadata(self.directory.join("captures.sqlite3-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        anyhow::ensure!(
            main <= MAX_MAIN_BYTES && wal <= MAX_WAL_BYTES,
            "captured history physical quota exceeded"
        );
        let (busy, _, _): (i64, i64, i64) =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        // Do not keep growing a pinned WAL. Retry requires a controlled restart.
        anyhow::ensure!(busy == 0, "captured history checkpoint is busy");
        Ok(())
    }
}

fn prune_transaction(transaction: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    loop {
        let (count, bytes): (i64, i64) = transaction.query_row(
            "SELECT count(*), coalesce(sum(bytes),0) FROM captures",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count <= MAX_RECORDS && bytes <= MAX_PAYLOAD_BYTES {
            break;
        }
        transaction.execute("DELETE FROM captures WHERE identity=(SELECT identity FROM captures ORDER BY updated,identity LIMIT 1)", [])?;
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> anyhow::Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    anyhow::ensure!(
        (0..=1).contains(&version),
        "captured history schema is newer than this gateway"
    );
    let check: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    anyhow::ensure!(check == "ok", "captured history database is corrupt");
    if version == 0 {
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(tables == 0, "unrecognized captured history schema");
        let transaction = connection.transaction()?;
        transaction.execute_batch("CREATE TABLE backends (session TEXT PRIMARY KEY, endpoint TEXT NOT NULL, incarnation TEXT NOT NULL);
            CREATE TABLE captures (identity TEXT PRIMARY KEY, policy TEXT NOT NULL, rows BLOB NOT NULL, bytes INTEGER NOT NULL, updated INTEGER NOT NULL);
            CREATE INDEX capture_retention ON captures(updated);
            PRAGMA user_version=1;")?;
        transaction.commit()?;
    }
    Ok(())
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn encode_rows(rows: &[String]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(rows.iter().map(|row| row.len() + 4).sum());
    for row in rows {
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

#[cfg(unix)]
fn secure_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            // SAFETY: geteuid has no arguments or memory requirements.
            anyhow::ensure!(
                metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
                "unsafe history directory"
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path, create: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments or memory requirements.
    anyhow::ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1,
        "unsafe history file"
    );
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("secure history storage is supported on Unix only")
}
#[cfg(not(unix))]
fn secure_file(_path: &Path, _create: bool) -> anyhow::Result<()> {
    anyhow::bail!("secure history storage is supported on Unix only")
}

#[cfg(test)]
mod tests {
    use super::super::backend::{AgentStatus, Pane, PaneId, TabId, WorkspaceId};
    use super::super::history::read_page;
    use super::*;
    use serde_json::json;

    #[test]
    fn migration_failure_rolls_back_schema_and_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection.pragma_update(None, "max_page_count", 1).unwrap();
        assert!(migrate(&mut connection).is_err());
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='table'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        connection
            .pragma_update(None, "max_page_count", 100)
            .unwrap();
        migrate(&mut connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    struct Fixture {
        directory: PathBuf,
        config: Config,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".cache/tmp/opencode")
                .join(format!("sqlite-{}", Uuid::new_v4().simple()));
            std::fs::create_dir_all(&directory).unwrap();
            let mut config = crate::test_config("fixture-only");
            config.history.storage = HistoryStorage::Sqlite;
            config.sessions[0].socket_path = directory
                .join("isolated.sock")
                .to_string_lossy()
                .into_owned();
            Self { directory, config }
        }
        fn history(&self) -> PathBuf {
            self.directory.join("history")
        }
        async fn open(&self, captures: Arc<Mutex<ScrollbackStore>>) -> SqliteHistoryRepository {
            SqliteHistoryRepository::open(Ok(self.history()), self.config.clone(), captures).await
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn pane(identity: Option<&str>) -> Pane {
        Pane {
            history_identity: identity.map(str::to_owned),
            id: PaneId::new("p"),
            terminal_id: Some("terminal".into()),
            workspace_id: WorkspaceId::new("w"),
            tab_id: TabId::new("t"),
            label: None,
            terminal_title: None,
            cwd: None,
            focused: false,
            width: Some(80),
            height: Some(4),
            revision: None,
            foreground_command: Some("claude".into()),
            agent: None,
            agent_status: AgentStatus::Unknown,
            max_offset_from_bottom: Some(0),
            viewport_rows: Some(4),
            alternate_on: Some(true),
            cursor_x: None,
            cursor_y: None,
        }
    }
    fn scope() -> HistoryScope {
        HistoryScope {
            session: "default".into(),
            pane: "p".into(),
            format: "text".into(),
            device: "d".into(),
            generation: "g".into(),
            limit: 2,
        }
    }
    fn captures(identity: Option<&str>, feed: bool) -> Arc<Mutex<ScrollbackStore>> {
        let mut store = ScrollbackStore::default();
        store.observe_native_listing("default", &[pane(identity)]);
        if feed {
            for top in 0..8 {
                store.record_frame(
                    "default",
                    "p",
                    "recent_unwrapped",
                    "text",
                    &(top..top + 4)
                        .map(|n| format!("row {n}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        Arc::new(Mutex::new(store))
    }

    fn feed_ansi(store: &mut ScrollbackStore) {
        for top in 0..8 {
            store.record_frame("default", "p", "recent_unwrapped", "ansi", &ansi_frame(top));
        }
    }

    fn ansi_frame(top: usize) -> String {
        (top..top + 4)
            .map(|n| format!("\u{1b}[31mrow {n}\u{1b}[0m"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn numbered_frame(from: usize, count: usize, size: usize, format: &str) -> String {
        (from..from + count)
            .map(|n| {
                let row = format!("{n:08} {}", "x".repeat(size - 9));
                if format == "ansi" {
                    format!("\u{1b}[31m{row}\u{1b}[0m")
                } else {
                    row
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn sqlite_byte_and_line_caps_slide_cursors_and_recover_only_the_retained_projection() {
        for format in ["text", "ansi"] {
            for byte_cap in [true, false] {
                let fixture = Fixture::new();
                let fold = captures(Some("A"), false);
                let mut scope = scope();
                scope.format = format.into();
                scope.limit = 500;
                let size = if byte_cap { 1024 } else { 32 };
                let count = if byte_cap {
                    MAX_PANE_BYTES / size + 200
                } else {
                    MAX_PANE_LINES + 200
                };
                let frame = |from, count| numbered_frame(from, count, size, format);
                let feed = |text: &str| {
                    fold.lock().unwrap().record_frame(
                        "default",
                        "p",
                        "recent_unwrapped",
                        format,
                        text,
                    );
                };
                feed(&frame(0, count));
                feed(&frame(500_000, 4));
                let repository = fixture.open(fold.clone()).await;
                // A cap-full projection must be writable, not a healthy memory
                // page masking a worker failure caused by encoding overhead.
                persisted_scope(&repository, &scope).await;
                let before = repository.capture(&scope, true).await.unwrap().unwrap();
                let metadata = repository.capture(&scope, false).await.unwrap().unwrap();
                assert_eq!(metadata.len, before.rows.len());
                assert_eq!(metadata.trimmed, before.trimmed);
                assert!(metadata.rows.is_empty());
                assert!(before.trimmed > 0);
                if byte_cap {
                    assert!(encode_rows(&before.rows).len() > MAX_PANE_BYTES);
                } else {
                    assert_eq!(before.len, MAX_PANE_LINES - 4);
                }

                let first = read_page(&repository, scope.clone(), None).await.unwrap();
                assert_eq!(
                    read_page(&repository, scope.clone(), None).await.unwrap(),
                    first
                );
                let mut page = first.clone();
                let mut cursors = Vec::new();
                while let Some(cursor) = page["next_before"].as_str().map(str::to_owned) {
                    page = read_page(&repository, scope.clone(), Some(&cursor))
                        .await
                        .unwrap();
                    cursors.push(cursor);
                }
                assert!(cursors.len() >= 4);
                let fence = fold.lock().unwrap().begin_capture("default", "p");
                feed(&frame(1_000_000, 4));
                feed(&frame(2_000_000, 4));
                let after = repository.capture(&scope, true).await.unwrap().unwrap();
                assert_eq!(after.epoch, before.epoch);
                assert_eq!(after.trimmed, before.trimmed + 8);
                assert_eq!(after.len, before.len);
                assert_eq!(fold.lock().unwrap().begin_capture("default", "p"), fence);
                // Front trims keep still-held pages alive, not the oldest page
                // whose first row was dropped. Gone removes the whole snapshot.
                read_page(&repository, scope.clone(), Some(&cursors[0]))
                    .await
                    .unwrap();
                assert_eq!(
                    read_page(
                        &repository,
                        scope.clone(),
                        cursors.last().map(String::as_str)
                    )
                    .await,
                    Err(HistoryError::Gone)
                );
                assert_eq!(
                    read_page(&repository, scope.clone(), Some(&cursors[0])).await,
                    Err(HistoryError::Gone)
                );
                let fresh = read_page(&repository, scope.clone(), None).await.unwrap();
                assert_ne!(fresh["snapshot_id"], first["snapshot_id"]);
                // A cursor remains pinned, but a new traversal must not reuse
                // its same-epoch snapshot after append/trim has moved the window.
                feed(&frame(2_000_001, 4));
                read_page(&repository, scope.clone(), fresh["next_before"].as_str())
                    .await
                    .unwrap();
                let current = read_page(&repository, scope.clone(), None).await.unwrap();
                assert_ne!(current["snapshot_id"], fresh["snapshot_id"]);
                assert_eq!(
                    read_page(&repository, scope.clone(), None).await.unwrap(),
                    current
                );
                let after = repository.capture(&scope, true).await.unwrap().unwrap();
                assert_eq!(after.epoch, before.epoch);
                assert_eq!(after.trimmed, before.trimmed + 9);
                assert_eq!(
                    current["rows"].as_array().unwrap().last().unwrap(),
                    after.rows.last().unwrap()
                );
                let old_cursor = current["next_before"].as_str().unwrap().to_owned();
                let mut page = current;
                let mut tiled = Vec::new();
                loop {
                    let mut rows: Vec<String> =
                        serde_json::from_value(page["rows"].clone()).unwrap();
                    rows.extend(tiled);
                    tiled = rows;
                    let Some(cursor) = page["next_before"].as_str() else {
                        break;
                    };
                    page = read_page(&repository, scope.clone(), Some(cursor))
                        .await
                        .unwrap();
                }
                assert_eq!(tiled, after.rows);
                persisted_scope(&repository, &scope).await;
                repository.close().await;

                let restarted = captures(Some("A"), false);
                let repository = fixture.open(restarted.clone()).await;
                assert_eq!(
                    read_page(&repository, scope.clone(), Some(&old_cursor)).await,
                    Err(HistoryError::Gone)
                );
                let checkpoint = read_page(&repository, scope.clone(), None).await.unwrap();
                let recovered = repository.capture(&scope, true).await.unwrap().unwrap();
                assert_eq!(recovered.rows, after.rows);
                assert_eq!(recovered.trimmed, 0);
                assert_eq!(recovered.len, after.len);
                assert_ne!(recovered.epoch, after.epoch);
                let metadata = repository.capture(&scope, false).await.unwrap().unwrap();
                assert_eq!(metadata.len, after.len);
                assert_eq!(metadata.trimmed, 0);
                assert!(metadata.rows.is_empty());
                assert_eq!(restarted.lock().unwrap().depth("default", "p"), 0);
                restarted.lock().unwrap().record_frame(
                    "default",
                    "p",
                    "recent_unwrapped",
                    format,
                    &frame(3_000_000, 4),
                );
                assert_eq!(
                    read_page(&repository, scope.clone(), None).await.unwrap(),
                    checkpoint
                );
                restarted.lock().unwrap().record_frame(
                    "default",
                    "p",
                    "recent_unwrapped",
                    format,
                    &frame(3_000_001, 4),
                );
                assert_eq!(
                    read_page(
                        &repository,
                        scope.clone(),
                        checkpoint["next_before"].as_str()
                    )
                    .await,
                    Err(HistoryError::Gone)
                );
                let replacement = repository.capture(&scope, true).await.unwrap().unwrap();
                assert_eq!(replacement.rows, vec![frame(3_000_000, 1)]);
                assert_ne!(replacement.epoch, recovered.epoch);
                persisted_scope(&repository, &scope).await;
                repository.close().await;
                let repository = fixture.open(captures(Some("A"), false)).await;
                assert_eq!(
                    read_page(&repository, scope.clone(), None).await.unwrap()["rows"],
                    json!(replacement.rows)
                );
                repository.close().await;
            }
        }
    }

    #[test]
    fn sqlite_encoding_cap_counts_row_text_separately_and_rejects_excess() {
        let rows = vec!["x".repeat(1024); MAX_PANE_BYTES / 1024];
        let encoded = encode_rows(&rows);
        assert!(encoded.len() > MAX_PANE_BYTES);
        assert_eq!(decode_rows(&encoded).unwrap(), rows);
        let mut excess = rows;
        excess.push("x".into());
        assert!(decode_rows(&encode_rows(&excess)).is_err());
        assert!(decode_rows(&encode_rows(&vec![String::new(); MAX_PANE_LINES + 1])).is_err());
        assert!(decode_rows(&vec![0; MAX_ENCODED_BYTES + 1]).is_err());
    }

    #[test]
    fn unknown_to_verified_identity_quarantines_rows_and_inflight_fences_before_sqlite_recovery() {
        for transitions in [
            vec![None, Some("B")],
            vec![Some("A"), None, Some("A")],
            vec![Some("A"), None, Some("B")],
        ] {
            let fixture = Fixture::new();
            let fold = captures(transitions[0], true);
            feed_ansi(&mut fold.lock().unwrap());
            let mut worker =
                Worker::open(fixture.history(), &fixture.config, fold.clone()).unwrap();
            for _ in 0..4 {
                worker.flush().unwrap();
            }
            assert_eq!(
                worker
                    .connection
                    .query_row("SELECT count(*) FROM captures", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                2
            );
            for (index, identity) in transitions.iter().enumerate().skip(1) {
                {
                    let mut store = fold.lock().unwrap();
                    let old_fence = store.begin_capture("default", "p");
                    assert!(old_fence.is_some());
                    store.observe_native_listing("default", &[pane(*identity)]);
                    assert_ne!(store.begin_capture("default", "p"), old_fence);
                    let raw = "unverified late 0\nunverified late 1\nunverified late 2\nunverified late 3";
                    assert_eq!(
                        store.serve_read_fenced(
                            "default",
                            "p",
                            ("recent_unwrapped", "text"),
                            raw,
                            200,
                            old_fence
                        ),
                        raw
                    );
                    store.record_frame_fenced(
                        "default",
                        "p",
                        "recent_unwrapped",
                        "ansi",
                        raw,
                        old_fence,
                    );
                    for format in ["text", "ansi"] {
                        let mut scope = scope();
                        scope.format = format.into();
                        assert!(store.live_captured_history(&scope, true).unwrap().is_none());
                    }
                }
                for _ in 0..4 {
                    worker.flush().unwrap();
                }
                assert_eq!(
                    worker
                        .connection
                        .query_row("SELECT count(*) FROM captures", [], |row| row
                            .get::<_, i64>(0))
                        .unwrap(),
                    0
                );
                if index + 1 < transitions.len() {
                    // Capture another prefix during the unknown interval; it too
                    // must be discarded when the same or another process verifies.
                    let mut store = fold.lock().unwrap();
                    store.record_frame(
                        "default",
                        "p",
                        "recent_unwrapped",
                        "text",
                        "unknown 0\nunknown 1\nunknown 2\nunknown 3",
                    );
                    store.record_frame(
                        "default",
                        "p",
                        "recent_unwrapped",
                        "text",
                        "unknown 1\nunknown 2\nunknown 3\nunknown 4",
                    );
                    drop(store);
                    for _ in 0..4 {
                        worker.flush().unwrap();
                    }
                    assert_eq!(
                        worker
                            .connection
                            .query_row("SELECT count(*) FROM captures", [], |row| row
                                .get::<_, i64>(0))
                            .unwrap(),
                        1
                    );
                }
            }
            drop(worker);
            let restarted = captures(*transitions.last().unwrap(), false);
            let worker =
                Worker::open(fixture.history(), &fixture.config, restarted.clone()).unwrap();
            let stamp = restarted
                .lock()
                .unwrap()
                .projection_stamp(&scope())
                .unwrap();
            assert!(worker.read(&stamp).unwrap().is_none());
        }
    }

    #[test]
    fn sqlite_retention_and_quota_eviction_do_not_resurrect_unchanged_polled_projections() {
        for expire in [true, false] {
            let fixture = Fixture::new();
            let fold = captures(Some("A"), true);
            feed_ansi(&mut fold.lock().unwrap());
            let mut worker =
                Worker::open(fixture.history(), &fixture.config, fold.clone()).unwrap();
            let base = now_seconds();
            for _ in 0..4 {
                worker.flush_at(base).unwrap();
            }
            let mut ansi = scope();
            ansi.format = "ansi".into();
            let scopes = [scope(), ansi];
            let original: Vec<_> = scopes
                .iter()
                .map(|scope| {
                    fold.lock()
                        .unwrap()
                        .live_captured_history(scope, true)
                        .unwrap()
                        .unwrap()
                })
                .collect();
            let now = if expire {
                let expired = base + RETENTION_SECONDS + 1;
                worker.prune_at(expired).unwrap();
                expired
            } else {
                for n in 0..MAX_RECORDS {
                    let encoded = encode_rows(&["quota fixture".into()]);
                    worker
                        .connection
                        .execute(
                            "INSERT INTO captures VALUES (?1,'quota',?2,?3,?4)",
                            params![format!("quota-{n}"), encoded, encoded.len(), base + 1],
                        )
                        .unwrap();
                }
                worker.prune_at(base + 1).unwrap();
                base + 2
            };
            for scope in &scopes {
                let stamp = fold.lock().unwrap().projection_stamp(scope).unwrap();
                assert!(worker.read(&stamp).unwrap().is_none());
            }
            for _ in 0..3 {
                {
                    let mut store = fold.lock().unwrap();
                    store.record_frame(
                        "default",
                        "p",
                        "recent_unwrapped",
                        "text",
                        "row 7\nrow 8\nrow 9\nrow 10",
                    );
                    store.record_frame("default", "p", "recent_unwrapped", "ansi", &ansi_frame(7));
                }
                for _ in 0..4 {
                    worker.flush_at(now).unwrap();
                }
                for (scope, original) in scopes.iter().zip(&original) {
                    let store = fold.lock().unwrap();
                    let capture = store.live_captured_history(scope, true).unwrap().unwrap();
                    assert_eq!(capture.rows, original.rows);
                    assert_eq!(capture.epoch, original.epoch);
                    let stamp = store.projection_stamp(scope).unwrap();
                    drop(store);
                    assert!(
                        worker.read(&stamp).unwrap().is_none(),
                        "unchanged poll resurrected an evicted projection"
                    );
                }
            }
            {
                let mut store = fold.lock().unwrap();
                store.record_frame(
                    "default",
                    "p",
                    "recent_unwrapped",
                    "text",
                    "row 8\nrow 9\nrow 10\nrow 11",
                );
            }
            for _ in 0..4 {
                worker.flush_at(now + 1).unwrap();
            }
            let text_stamp = fold.lock().unwrap().projection_stamp(&scopes[0]).unwrap();
            let (rows, _) = worker.read(&text_stamp).unwrap().unwrap();
            assert_eq!(
                rows,
                fold.lock()
                    .unwrap()
                    .live_captured_history(&scopes[0], true)
                    .unwrap()
                    .unwrap()
                    .rows
            );
            assert_ne!(rows, original[0].rows);
            let updated: i64 = worker
                .connection
                .query_row(
                    "SELECT updated FROM captures WHERE identity=?1",
                    [worker.key(&text_stamp).unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(updated, now + 1);
            let ansi_stamp = fold.lock().unwrap().projection_stamp(&scopes[1]).unwrap();
            assert!(
                worker.read(&ansi_stamp).unwrap().is_none(),
                "text change renewed ANSI retention"
            );
            fold.lock().unwrap().record_frame(
                "default",
                "p",
                "recent_unwrapped",
                "ansi",
                &ansi_frame(8),
            );
            for _ in 0..4 {
                worker.flush_at(now + 2).unwrap();
            }
            let ansi_stamp = fold.lock().unwrap().projection_stamp(&scopes[1]).unwrap();
            assert!(worker.read(&ansi_stamp).unwrap().is_some());
            assert_eq!(
                worker
                    .connection
                    .query_row(
                        "SELECT updated FROM captures WHERE identity=?1",
                        [worker.key(&ansi_stamp).unwrap()],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                now + 2
            );
            assert_eq!(
                worker
                    .connection
                    .query_row(
                        "SELECT updated FROM captures WHERE identity=?1",
                        [worker.key(&text_stamp).unwrap()],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                updated
            );
        }
    }
    async fn persisted(repository: &SqliteHistoryRepository) {
        persisted_scope(repository, &scope()).await;
    }

    async fn persisted_scope(repository: &SqliteHistoryRepository, scope: &HistoryScope) {
        let stamp = repository
            .captures
            .lock()
            .unwrap()
            .projection_stamp(scope)
            .unwrap();
        let expected = repository
            .captures
            .lock()
            .unwrap()
            .live_captured_history(scope, true)
            .unwrap()
            .unwrap()
            .rows;
        for _ in 0..100 {
            let (reply, receive) = oneshot::channel();
            repository
                .send(Command::Read(stamp.clone(), reply))
                .unwrap();
            if receive
                .await
                .unwrap()
                .unwrap()
                .is_some_and(|(rows, _)| rows == expected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("projection was not persisted");
    }

    #[tokio::test]
    async fn real_worker_close_reopen_recovers_checkpoint_not_viewport_and_old_cursor_is_gone() {
        let fixture = Fixture::new();
        let repository = fixture.open(captures(Some("verified-root-A"), true)).await;
        persisted(&repository).await;
        let (first, concurrent) = tokio::join!(
            read_page(&repository, scope(), None),
            read_page(&repository, scope(), None)
        );
        let first = first.unwrap();
        assert_eq!(first, concurrent.unwrap());
        let cursor = first["next_before"].as_str().unwrap().to_owned();
        repository.close().await;
        let fold = captures(Some("verified-root-A"), false);
        let repository = fixture.open(fold.clone()).await;
        let mut restarted = scope();
        restarted.generation = "new-run".into();
        assert_eq!(
            read_page(&repository, restarted.clone(), Some(&cursor)).await,
            Err(HistoryError::Gone)
        );
        let checkpoint = read_page(&repository, restarted.clone(), None)
            .await
            .unwrap();
        assert_eq!(checkpoint["rows"], json!(["row 5", "row 6"]));
        assert_ne!(checkpoint["capture_epoch"], first["capture_epoch"]);
        assert_eq!(fold.lock().unwrap().depth("default", "p"), 0);
        {
            let mut store = fold.lock().unwrap();
            store.record_frame(
                "default",
                "p",
                "recent_unwrapped",
                "text",
                "new 0\nnew 1\nnew 2\nnew 3",
            );
        }
        assert_eq!(
            read_page(&repository, restarted.clone(), None)
                .await
                .unwrap(),
            checkpoint
        );
        {
            let mut store = fold.lock().unwrap();
            store.record_frame(
                "default",
                "p",
                "recent_unwrapped",
                "text",
                "new 1\nnew 2\nnew 3\nnew 4",
            );
        }
        assert_eq!(
            read_page(&repository, restarted, None).await.unwrap()["rows"],
            json!(["new 0"])
        );
        persisted(&repository).await;
        repository.close().await;
        let repository = fixture.open(captures(Some("verified-root-A"), false)).await;
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["rows"],
            json!(["new 0"])
        );
        repository.close().await;
    }

    #[tokio::test]
    async fn cross_process_unknown_install_endpoint_and_config_membership_fail_closed() {
        let mut fixture = Fixture::new();
        let repository = fixture.open(captures(Some("A"), true)).await;
        persisted(&repository).await;
        repository.close().await;
        for identity in [None, Some("B")] {
            let repository = fixture.open(captures(identity, false)).await;
            assert_eq!(
                read_page(&repository, scope(), None).await.unwrap()["availability"],
                "not_captured"
            );
            repository.close().await;
        }
        let initial = fixture.config.clone();
        for change in 0..3 {
            fixture.config = initial.clone();
            match change {
                0 => fixture.config.server_id = "other-install".into(),
                1 => fixture.config.sessions[0].socket_path.push_str(".changed"),
                _ => fixture.config.sessions.clear(),
            }
            let repository = fixture.open(captures(Some("A"), false)).await;
            assert_eq!(
                read_page(&repository, scope(), None).await.unwrap()["availability"],
                "not_captured"
            );
            repository.close().await;
            fixture.config = initial.clone();
            let repository = fixture.open(captures(Some("A"), false)).await;
            assert_eq!(
                read_page(&repository, scope(), None).await.unwrap()["availability"],
                "not_captured"
            );
            repository.close().await;
        }
    }

    #[test]
    fn checkpoint_read_fence_rejects_delayed_recovery_after_reset_or_revision_change() {
        for reset in 0..4 {
            let store = captures(Some("A"), false);
            let mut store = store.lock().unwrap();
            let stamp = store.projection_stamp(&scope()).unwrap();
            match reset {
                0 => {
                    let mut p = pane(Some("A"));
                    p.width = Some(120);
                    store.observe_native_listing("default", &[p]);
                }
                1 => store.observe_native_listing("default", &[pane(Some("B"))]),
                2 => store.observe_listing("default", &json!({"panes": []})),
                _ => store.record_frame("default", "p", "recent_unwrapped", "text", "screen"),
            }
            assert!(!store.recover_checkpoint(
                &stamp,
                vec!["old".into()],
                Instant::now() + Duration::from_secs(3600)
            ));
        }
        let store = captures(Some("A"), false);
        let mut store = store.lock().unwrap();
        let stamp = store.projection_stamp(&scope()).unwrap();
        assert!(!store.recover_checkpoint(&stamp, vec!["expired".into()], Instant::now()));
        assert!(store.recover_checkpoint(
            &stamp,
            vec!["checkpoint".into()],
            Instant::now() + Duration::from_secs(3600)
        ));
        assert!(store
            .live_captured_history(&scope(), true)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn sqlite_formats_are_independent_and_destructive_shrink_removes_durable_projection() {
        let fixture = Fixture::new();
        let fold = captures(Some("A"), true);
        {
            let mut store = fold.lock().unwrap();
            for top in 0..8 {
                store.record_frame(
                    "default",
                    "p",
                    "recent_unwrapped",
                    "ansi",
                    &(top..top + 4)
                        .map(|n| format!("\u{1b}[31mrow {n}\u{1b}[0m"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        let repository = fixture.open(fold).await;
        let mut ansi = scope();
        ansi.format = "ansi".into();
        persisted_scope(&repository, &ansi).await;
        persisted(&repository).await;
        repository.close().await;
        let fold = captures(Some("A"), false);
        let repository = fixture.open(fold.clone()).await;
        assert_eq!(
            read_page(&repository, ansi.clone(), None).await.unwrap()["rows"],
            json!(["\u{1b}[31mrow 5\u{1b}[0m", "\u{1b}[31mrow 6\u{1b}[0m"])
        );
        let first = read_page(&repository, scope(), None).await.unwrap();
        assert_eq!(first["rows"], json!(["row 5", "row 6"]));
        {
            let mut store = fold.lock().unwrap();
            let mut editor = pane(Some("A"));
            editor.foreground_command = Some("nvim".into());
            store.observe_native_listing("default", &[editor]);
            store.record_frame("default", "p", "recent_unwrapped", "text", "editor only");
        }
        assert_eq!(
            read_page(&repository, scope(), first["next_before"].as_str()).await,
            Err(HistoryError::Gone)
        );
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["rows"],
            json!([])
        );
        // Repository operations give both dirty shapes a turn on the real worker.
        for _ in 0..4 {
            repository.health().await.unwrap();
        }
        repository.close().await;
        let directory = fixture.history();
        let count = tokio::task::spawn_blocking(move || {
            Connection::open(directory.join("captures.sqlite3"))
                .unwrap()
                .query_row("SELECT count(*) FROM captures", [], |r| r.get::<_, i64>(0))
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn initialization_corrupt_future_schema_and_worker_loss_are_visible_and_preserve_files() {
        for future in [false, true] {
            let fixture = Fixture::new();
            secure_directory(&fixture.history()).unwrap();
            let path = fixture.history().join("captures.sqlite3");
            if future {
                let connection = Connection::open(&path).unwrap();
                connection.pragma_update(None, "user_version", 99).unwrap();
            } else {
                std::fs::write(&path, b"not a sqlite database").unwrap();
            }
            let original = std::fs::read(&path).unwrap();
            let repository = fixture.open(captures(Some("A"), true)).await;
            assert_eq!(
                read_page(&repository, scope(), None).await,
                Err(HistoryError::Unavailable)
            );
            repository.close().await;
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        let fixture = Fixture::new();
        let mut repository = fixture.open(captures(Some("A"), true)).await;
        persisted(&repository).await;
        let first = read_page(&repository, scope(), None).await.unwrap();
        repository.commands.take();
        assert_eq!(
            read_page(&repository, scope(), first["next_before"].as_str()).await,
            Err(HistoryError::Unavailable)
        );
        repository.close().await;
    }

    #[test]
    fn bounded_channel_overload_and_dead_worker_are_not_memory_success() {
        let (send, receive) = mpsc::sync_channel(1);
        let repository = SqliteHistoryRepository {
            memory: MemoryHistoryRepository::new(captures(Some("A"), true)),
            captures: captures(Some("A"), true),
            commands: Some(send),
            worker: None,
        };
        let (reply, _) = oneshot::channel();
        repository.send(Command::Health(reply)).unwrap();
        let (reply, _) = oneshot::channel();
        assert!(matches!(
            repository.send(Command::Health(reply)),
            Err(HistoryError::Unavailable)
        ));
        drop(receive);
        let (reply, _) = oneshot::channel();
        assert!(matches!(
            repository.send(Command::Health(reply)),
            Err(HistoryError::Unavailable)
        ));
    }

    #[test]
    fn retention_logical_physical_quotas_permissions_and_busy_checkpoint() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let mut worker = Worker::open(
            fixture.history(),
            &fixture.config,
            captures(Some("A"), true),
        )
        .unwrap();
        for _ in 0..4 {
            worker.flush().unwrap();
        }
        worker
            .connection
            .execute("UPDATE captures SET updated=?1", [now_seconds() - 1000])
            .unwrap();
        let updated: i64 = worker
            .connection
            .query_row("SELECT updated FROM captures LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let stamp = worker
            .captures
            .lock()
            .unwrap()
            .projection_stamp(&scope())
            .unwrap();
        assert!(worker.read(&stamp).unwrap().is_some());
        assert_eq!(
            worker
                .connection
                .query_row("SELECT updated FROM captures LIMIT 1", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            updated
        );
        for n in 0..60 {
            worker
                .connection
                .execute(
                    "INSERT INTO captures VALUES (?1,'policy',?2,?3,?4)",
                    params![
                        format!("fixture-{n}"),
                        encode_rows(&["x".repeat(500_000)]),
                        500_004,
                        updated
                    ],
                )
                .unwrap();
            worker.checkpoint().unwrap();
        }
        worker.prune().unwrap();
        let (count, bytes): (i64, i64) = worker
            .connection
            .query_row("SELECT count(*),sum(bytes) FROM captures", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(count <= MAX_RECORDS && bytes <= MAX_PAYLOAD_BYTES);
        worker
            .connection
            .execute(
                "UPDATE captures SET updated=?1",
                [now_seconds() - RETENTION_SECONDS - 1],
            )
            .unwrap();
        worker.prune().unwrap();
        assert_eq!(
            worker
                .connection
                .query_row("SELECT count(*) FROM captures", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        for (name, mode) in [
            ("", 0o700),
            ("captures.sqlite3", 0o600),
            ("captures.sqlite3-wal", 0o600),
            ("captures.sqlite3-shm", 0o600),
        ] {
            assert_eq!(
                std::fs::metadata(fixture.history().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                mode
            );
        }
        assert!(
            std::fs::metadata(fixture.history().join("captures.sqlite3"))
                .unwrap()
                .len()
                <= MAX_MAIN_BYTES
        );
        assert!(
            std::fs::metadata(fixture.history().join("captures.sqlite3-wal"))
                .unwrap()
                .len()
                <= MAX_WAL_BYTES
        );
        let other = Connection::open(fixture.history().join("captures.sqlite3")).unwrap();
        other
            .execute_batch("BEGIN; SELECT count(*) FROM captures;")
            .unwrap();
        worker
            .connection
            .execute(
                "INSERT INTO captures VALUES ('busy','p',X'','0',?1)",
                [now_seconds()],
            )
            .unwrap();
        assert!(worker.checkpoint().is_err());
        other.execute_batch("ROLLBACK").unwrap();
        worker.checkpoint().unwrap();
        worker.connection.execute_batch("VACUUM").unwrap();
        worker.checkpoint().unwrap();
        worker
            .connection
            .pragma_update(None, "max_page_count", 1)
            .unwrap();
        let pages: u64 = worker
            .connection
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        worker
            .connection
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        assert!(worker
            .connection
            .execute(
                "INSERT INTO captures VALUES ('full','p',?1,?2,?3)",
                params![vec![0u8; MAX_PANE_BYTES], MAX_PANE_BYTES, now_seconds()]
            )
            .is_err());
        worker.checkpoint().unwrap();
    }

    #[test]
    fn private_files_reject_symlinks_hardlinks_and_nonregular_files_without_following_them() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let target = fixture.directory.join("keep");
        std::fs::write(&target, "keep").unwrap();
        symlink(&target, fixture.history()).unwrap();
        assert!(secure_directory(&fixture.history()).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        let link = fixture.directory.join("link");
        symlink(&target, &link).unwrap();
        assert!(secure_file(&link, true).is_err());
        let hard = fixture.directory.join("hard");
        std::fs::hard_link(&target, &hard).unwrap();
        assert!(secure_file(&hard, false).is_err());
        assert!(secure_file(&fixture.directory, false).is_err());
        for rows in [
            vec!["\u{1b}[31mansi\u{1b}[0m".into(), "".into(), "text".into()],
            vec!["x".repeat(super::super::history::MAX_PAGE_BYTES - 1)],
        ] {
            assert_eq!(decode_rows(&encode_rows(&rows)).unwrap(), rows);
        }
        for bad in [vec![1], vec![10, 0, 0, 0], vec![1, 0, 0, 0, 255]] {
            assert!(decode_rows(&bad).is_err());
        }
    }

    #[tokio::test]
    async fn default_memory_does_not_resolve_or_touch_an_existing_history_directory() {
        let fixture = Fixture::new();
        let mut config = fixture.config.clone();
        config.history.storage = HistoryStorage::Memory;
        // An unreadable/non-database fixture is irrelevant to memory selection.
        std::fs::write(fixture.directory.join("history"), "do not touch").unwrap();
        let repository = repository(&config, captures(Some("A"), true)).await;
        assert_eq!(
            read_page(repository.as_ref(), scope(), None).await.unwrap()["rows"],
            json!(["row 5", "row 6"])
        );
        assert_eq!(
            std::fs::read_to_string(fixture.directory.join("history")).unwrap(),
            "do not touch"
        );
        assert!(!fixture.directory.join("history/captures.sqlite3").exists());
    }

    #[tokio::test]
    #[ignore = "requires real Herdr; launches only a fixture HOME/config/private socket"]
    async fn real_isolated_herdr_native_identity_survives_gateway_adapter_restart() {
        use super::super::backend::{CreateWorkspace, HerdrBackend, TerminalBackend};
        use std::process::{Command as ProcessCommand, Stdio};
        struct Server(std::process::Child);
        impl Drop for Server {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let fixture = Fixture::new();
        let config_path = fixture.directory.join("herdr.toml");
        std::fs::write(&config_path, "").unwrap();
        let socket = fixture.directory.join("herdr.sock");
        let mut command = ProcessCommand::new("herdr");
        for name in [
            "TMUX",
            "HERDR_SESSION",
            "HERDR_SOCKET_PATH",
            "HERDR_CLIENT_SOCKET_PATH",
            "HERDR_PANE_ID",
            "HERDR_TAB_ID",
            "HERDR_WORKSPACE_ID",
            "HERDR_ENV",
        ] {
            command.env_remove(name);
        }
        command
            .env("HOME", &fixture.directory)
            .env("XDG_CONFIG_HOME", fixture.directory.join("config"))
            .env("HERDR_CONFIG_PATH", &config_path)
            .env("HERDR_SOCKET_PATH", &socket)
            .current_dir(&fixture.directory)
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(fixture.directory.join("herdr-stderr.log")).unwrap());
        let _server = Server(command.spawn().unwrap());
        let backend = HerdrBackend::new(&socket);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if backend.metadata().await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "isolated Herdr never became ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(fixture.directory.clone()),
                label: Some("history-identity-qa".into()),
                focus: false,
            })
            .await
            .unwrap();
        let first = backend.list_panes().await.unwrap();
        assert!(!first.is_empty());
        for pane in &first {
            assert!(
                pane.history_identity.is_some(),
                "Herdr peer/root process identity was not verified"
            );
        }
        let restarted_adapter = HerdrBackend::new(&socket);
        let second = restarted_adapter.list_panes().await.unwrap();
        for pane in first {
            let second = second.iter().find(|current| current.id == pane.id).unwrap();
            assert_eq!(pane.history_identity, second.history_identity);
            assert!(!super::super::backend::compat::pane_get(second.clone())
                .to_string()
                .contains("history_identity"));
        }
    }
}
