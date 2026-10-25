//! One SQLite connection on its own thread, shared by every domain that persists
//! something. Domains hand it closures; it hands back results. Namespaced
//! migrations let the history checkpoints, tasks and tokens share a file.
use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use super::db_key::DbKey;

/// One domain's schema history: `steps[i]` takes the namespace from version `i`
/// to `i + 1`. Append to a domain's steps, never edit one that has shipped.
pub(crate) struct Migration {
    pub namespace: &'static str,
    pub steps: &'static [&'static str],
}

/// Every domain that keeps rows in the shared store registers here.
pub(crate) const ALL_MIGRATIONS: &[Migration] = &[
    crate::agents::adapters::codex::native_sessions::MIGRATION,
    crate::agents::runtime::SESSION_ROUTES_MIGRATION,
    crate::connectivity::replay_store::MIGRATION,
    crate::tasks::store_sqlite::MIGRATION,
    crate::mcp::tokens::MIGRATION,
    crate::fleet::peers::MIGRATION,
    crate::fleet::home_grants::MIGRATION,
    crate::terminal::history_sqlite::MIGRATION,
    super::t3_credentials::MIGRATION,
];

type Boxed = Box<dyn Any + Send>;
type Job = Box<dyn FnOnce(&mut Connection) -> anyhow::Result<Boxed> + Send>;
type Reply = oneshot::Sender<anyhow::Result<Boxed>>;

pub(crate) struct SqliteStore {
    /// A bounded queue: a burst waits its turn asynchronously, never by
    /// blocking the tokio worker thread that sent it.
    commands: Option<mpsc::Sender<(Job, Reply)>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SqliteStore {
    pub(crate) fn open(
        path: &Path,
        key: Option<DbKey>,
        migrations: &[Migration],
    ) -> anyhow::Result<Arc<SqliteStore>> {
        if let Some(dir) = path.parent() {
            super::ensure_secret_dir(dir)?;
        }
        let mut connection = open_connection(path, key.as_ref())?;
        migrate(&mut connection, migrations)?;
        let (tx, mut rx) = mpsc::channel::<(Job, Reply)>(64);
        let worker = std::thread::Builder::new()
            .name("gateway-store".into())
            .spawn(move || {
                while let Some((job, reply)) = rx.blocking_recv() {
                    // A panicking job fails its own call, not the worker.
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&mut connection)
                    }))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("store job panicked")));
                    let _ = reply.send(out);
                }
            })?;
        Ok(Arc::new(SqliteStore {
            commands: Some(tx),
            worker: Some(worker),
        }))
    }

    /// Run `job` on the store thread and wait for its result.
    pub(crate) async fn call<T, F>(&self, job: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let boxed: Job = Box::new(move |c| job(c).map(|t| Box::new(t) as Boxed));
        self.commands
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("store worker is gone"))?
            .send((boxed, tx))
            .await
            .map_err(|_| anyhow::anyhow!("store worker is gone"))?;
        let out = rx
            .await
            .map_err(|_| anyhow::anyhow!("store worker dropped the reply"))??;
        Ok(*out.downcast::<T>().expect("job result type"))
    }
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        // Close the channel, then wait: the connection is closed (and the WAL
        // folded back) before the last handle is gone.
        self.commands.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn open_connection(path: &Path, key: Option<&DbKey>) -> anyhow::Result<Connection> {
    let connection = Connection::open(path)?;
    if let Some(key) = key {
        // A wrong key is reported as an error below; SQLCipher's own stderr
        // line about it is noise on top of that.
        connection.execute_batch("PRAGMA cipher_log_level = NONE;")?;
        connection.execute_batch(&format!("PRAGMA key = \"{}\";", key.pragma_value()))?;
    }
    // The first read is where a wrong key shows up ("file is not a database").
    if let Err(e) = connection.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        anyhow::bail!(
            "store key does not match the database at {} ({e}); the gateway never rewrites an existing store",
            path.display()
        );
    }
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    super::lock_down_secret_file(path)?;
    Ok(connection)
}

fn migrate(connection: &mut Connection, migrations: &[Migration]) -> anyhow::Result<()> {
    connection.execute(
        "CREATE TABLE IF NOT EXISTS muqun_schema(namespace TEXT PRIMARY KEY, version INTEGER NOT NULL)",
        [],
    )?;
    for m in migrations {
        let version: i64 = match connection.query_row(
            "SELECT version FROM muqun_schema WHERE namespace=?1",
            [m.namespace],
            |r| r.get(0),
        ) {
            Ok(version) => version,
            Err(rusqlite::Error::QueryReturnedNoRows) => 0,
            Err(e) => return Err(e.into()),
        };
        let have = usize::try_from(version).unwrap_or(usize::MAX);
        anyhow::ensure!(
            have <= m.steps.len(),
            "schema for {} is newer than this gateway ({version} > {})",
            m.namespace,
            m.steps.len()
        );
        for (i, step) in m.steps.iter().enumerate().skip(have) {
            let tx = connection.transaction()?;
            tx.execute_batch(step)?;
            tx.execute(
                "INSERT INTO muqun_schema(namespace, version) VALUES(?1, ?2) ON CONFLICT(namespace) DO UPDATE SET version=excluded.version",
                rusqlite::params![m.namespace, (i + 1) as i64],
            )?;
            tx.commit()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const T: &[Migration] = &[Migration {
        namespace: "t",
        steps: &[
            "CREATE TABLE t(x INTEGER)",
            "ALTER TABLE t ADD COLUMN y INTEGER",
        ],
    }];

    fn temp_db(name: &str) -> PathBuf {
        let dir = crate::test_state_dir().join(format!("sqlite-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("gateway.sqlite3")
    }

    #[tokio::test]
    async fn migrations_apply_once_and_record_a_version() {
        let path = temp_db("migrate");
        let store = SqliteStore::open(&path, None, T).unwrap();
        let v: i64 = store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT version FROM muqun_schema WHERE namespace='t'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(v, 2);
        drop(store);
        let again = SqliteStore::open(&path, None, T).unwrap();
        let cols: i64 = again
            .call(|c| {
                Ok(
                    c.query_row("SELECT count(*) FROM pragma_table_info('t')", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(cols, 2);
    }

    #[tokio::test]
    async fn a_panicking_job_fails_that_call_and_the_worker_lives() {
        let store = SqliteStore::open(&temp_db("panic"), None, T).unwrap();
        let err = store
            .call(|_| -> anyhow::Result<()> { panic!("boom") })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("store job panicked"), "{err}");
        let one: i64 = store
            .call(|c| Ok(c.query_row("SELECT 1", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(one, 1);
    }

    #[tokio::test]
    async fn a_newer_schema_refuses_to_open() {
        let path = temp_db("newer");
        let store = SqliteStore::open(&path, None, T).unwrap();
        store
            .call(|c| {
                c.execute("UPDATE muqun_schema SET version=9 WHERE namespace='t'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        drop(store);
        let err = SqliteStore::open(&path, None, T).err().expect("refused");
        assert!(err.to_string().contains("newer than this gateway"), "{err}");
    }

    #[tokio::test]
    async fn the_wrong_key_is_reported_not_repaired() {
        let path = temp_db("key");
        let store = SqliteStore::open(&path, Some(DbKey::random()), T).unwrap();
        drop(store);
        let before = std::fs::metadata(&path).unwrap().len();
        let err = SqliteStore::open(&path, Some(DbKey::random()), T)
            .err()
            .expect("refused");
        let text = err.to_string();
        assert!(text.contains("store key does not match"), "{text}");
        assert!(
            text.contains("gateway.sqlite3"),
            "names the database: {text}"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before,
            "the database was left alone"
        );
    }

    #[tokio::test]
    async fn an_encrypted_file_is_not_plain_sqlite() {
        let path = temp_db("cipher");
        let _store = SqliteStore::open(&path, Some(DbKey::random()), T).unwrap();
        let head = std::fs::read(&path).unwrap();
        assert!(
            !head.starts_with(b"SQLite format 3"),
            "header must be encrypted"
        );
    }
}
