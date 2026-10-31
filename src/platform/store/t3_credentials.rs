//! Recoverable T3 credentials in the existing SQLCipher store. The legacy
//! private JSON file is removed only after a committed, verified import.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use rusqlite::{params, OptionalExtension as _};

use super::sqlite::{Migration, SqliteStore};
use super::{T3StoredCredential, T3_CREDENTIAL_FILE};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "t3_credentials",
    steps: &["CREATE TABLE t3_credentials (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
        url TEXT NOT NULL, token TEXT NOT NULL,
        saved_at_ms INTEGER NOT NULL CHECK(saved_at_ms >= 0)
    );"],
};

pub(crate) struct T3Credentials {
    db: Arc<SqliteStore>,
    legacy: PathBuf,
    migration: tokio::sync::Mutex<()>,
}

impl T3Credentials {
    /// Only attach this store when SQLCipher is enabled. Plain SQLite is an
    /// explicit owner configuration, not an encrypted credential store.
    pub(crate) fn new(db: Arc<SqliteStore>, directory: PathBuf) -> Self {
        Self {
            db,
            legacy: directory.join(T3_CREDENTIAL_FILE),
            migration: tokio::sync::Mutex::new(()),
        }
    }

    async fn read(&self) -> anyhow::Result<Option<T3StoredCredential>> {
        self.db
            .call(|db| {
                Ok(db
                    .query_row(
                        "SELECT url, token, saved_at_ms FROM t3_credentials WHERE singleton = 1",
                        [],
                        |row| {
                            let saved_at_ms: i64 = row.get(2)?;
                            let saved_at_ms = u64::try_from(saved_at_ms).map_err(|_| {
                                rusqlite::Error::IntegralValueOutOfRange(2, saved_at_ms)
                            })?;
                            Ok(T3StoredCredential {
                                url: row.get(0)?,
                                token: row.get(1)?,
                                saved_at_ms,
                            })
                        },
                    )
                    .optional()?)
            })
            .await
    }

    async fn write(&self, credential: &T3StoredCredential) -> anyhow::Result<()> {
        let credential = credential.clone();
        let saved_at_ms = i64::try_from(credential.saved_at_ms)
            .context("T3 credential timestamp is out of range")?;
        self.db
            .call(move |db| {
                db.execute(
                    "INSERT INTO t3_credentials (singleton, url, token, saved_at_ms)
                 VALUES(1, ?1, ?2, ?3) ON CONFLICT(singleton) DO UPDATE SET
                 url = excluded.url, token = excluded.token, saved_at_ms = excluded.saved_at_ms",
                    params![credential.url, credential.token, saved_at_ms],
                )?;
                Ok(())
            })
            .await
    }

    async fn import_if_empty(&self, credential: &T3StoredCredential) -> anyhow::Result<()> {
        let credential = credential.clone();
        let saved_at_ms = i64::try_from(credential.saved_at_ms)
            .context("T3 credential timestamp is out of range")?;
        self.db
            .call(move |db| {
                // Another Gateway instance can finish an import or save
                // after our initial read. Migration must never replace it.
                db.execute(
                    "INSERT INTO t3_credentials (singleton, url, token, saved_at_ms)
                     VALUES(1, ?1, ?2, ?3) ON CONFLICT(singleton) DO NOTHING",
                    params![credential.url, credential.token, saved_at_ms],
                )?;
                Ok(())
            })
            .await
    }

    async fn load_inner(&self) -> anyhow::Result<Option<T3StoredCredential>> {
        let stored = self.read().await?;
        let bytes = match std::fs::read(&self.legacy) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(stored),
            Err(error) => return Err(error).context("could not read legacy T3 credential"),
        };
        let legacy: T3StoredCredential =
            serde_json::from_slice(&bytes).context("could not decode legacy T3 credential")?;
        match &stored {
            Some(stored) if stored == &legacy => {}
            // `muqun-gateway agent setup t3` writes this file; the running
            // Gateway only ever reads the encrypted row. A file saved after
            // the row is the operator's newer credential (a refreshed bearer
            // or a new URL) and replaces it. Anything else -- the same
            // timestamp, or an older file -- is refused unchanged.
            Some(stored) if legacy.saved_at_ms > stored.saved_at_ms => {
                self.write(&legacy).await?;
            }
            Some(_) => anyhow::bail!(
                "legacy and encrypted T3 credentials differ and the file is not newer; neither was changed"
            ),
            None => self.import_if_empty(&legacy).await?,
        }
        // A separate read after the write has committed also handles a crash
        // between committing the import and removing its plaintext source.
        anyhow::ensure!(
            self.read().await?.as_ref() == Some(&legacy),
            "could not verify encrypted T3 credential import"
        );
        anyhow::ensure!(
            std::fs::read(&self.legacy).context("could not verify legacy T3 credential")? == bytes,
            "legacy T3 credential changed during import; source was kept"
        );
        std::fs::remove_file(&self.legacy).context("could not remove migrated T3 credential")?;
        Ok(Some(legacy))
    }

    pub(crate) async fn load(&self) -> anyhow::Result<Option<T3StoredCredential>> {
        let _one = self.migration.lock().await;
        self.load_inner().await
    }

    pub(crate) async fn save(&self, credential: &T3StoredCredential) -> anyhow::Result<()> {
        let _one = self.migration.lock().await;
        // Reconcile an old file before replacing anything. Failure never
        // silently overwrites or falls back to that plaintext file.
        self.load_inner().await?;
        self.write(credential).await?;
        anyhow::ensure!(
            self.read().await?.as_ref() == Some(credential),
            "could not verify stored T3 credential"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::store::{db_key::DbKey, write_t3_credential_at};

    fn fixture() -> (PathBuf, Arc<SqliteStore>, T3Credentials) {
        let directory =
            crate::test_state_dir().join(format!("t3-secrets-{}", uuid::Uuid::new_v4()));
        let db = SqliteStore::open(
            &directory.join("fixture.sqlite3"),
            Some(DbKey::random()),
            &[MIGRATION],
        )
        .unwrap();
        let store = T3Credentials::new(db.clone(), directory.clone());
        (directory, db, store)
    }

    fn credential() -> T3StoredCredential {
        T3StoredCredential {
            url: "http://127.0.0.1:3773".into(),
            token: "synthetic-recoverable-secret".into(),
            saved_at_ms: 17,
        }
    }

    #[tokio::test]
    async fn legacy_import_is_verified_and_plaintext_is_removed() {
        let (directory, _db, store) = fixture();
        let credential = credential();
        write_t3_credential_at(&directory, &credential).unwrap();
        assert_eq!(store.load().await.unwrap(), Some(credential.clone()));
        assert!(!directory.join(T3_CREDENTIAL_FILE).exists());
        assert_eq!(store.load().await.unwrap(), Some(credential.clone()));
        for name in ["fixture.sqlite3", "fixture.sqlite3-wal"] {
            let bytes = std::fs::read(directory.join(name)).unwrap_or_default();
            assert!(!bytes
                .windows(credential.token.len())
                .any(|part| part == credential.token.as_bytes()));
        }
        let mut replacement = credential;
        replacement.token = "synthetic-replacement".into();
        store.save(&replacement).await.unwrap();
        assert_eq!(store.load().await.unwrap(), Some(replacement));
        assert!(!directory.join(T3_CREDENTIAL_FILE).exists());
    }

    #[tokio::test]
    async fn an_interrupted_import_reconciles_only_matching_credentials() {
        let (directory, _db, store) = fixture();
        let original = credential();
        store.save(&original).await.unwrap();
        write_t3_credential_at(&directory, &original).unwrap();
        assert_eq!(store.load().await.unwrap(), Some(original.clone()));
        assert!(!directory.join(T3_CREDENTIAL_FILE).exists());
        let mut different = original.clone();
        different.token = "synthetic-other".into();
        write_t3_credential_at(&directory, &different).unwrap();
        let before = std::fs::read(directory.join(T3_CREDENTIAL_FILE)).unwrap();
        assert!(store.load().await.is_err());
        assert!(store.save(&different).await.is_err());
        assert_eq!(store.read().await.unwrap(), Some(original));
        assert_eq!(
            std::fs::read(directory.join(T3_CREDENTIAL_FILE)).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn a_newer_setup_file_replaces_the_stored_credential() {
        let (directory, _db, store) = fixture();
        let original = credential();
        store.save(&original).await.unwrap();
        let mut refreshed = original.clone();
        refreshed.url = "http://127.0.0.1:3775".into();
        refreshed.token = "synthetic-refreshed".into();
        refreshed.saved_at_ms = original.saved_at_ms + 1;
        write_t3_credential_at(&directory, &refreshed).unwrap();
        assert_eq!(store.load().await.unwrap(), Some(refreshed.clone()));
        assert!(!directory.join(T3_CREDENTIAL_FILE).exists());
        assert_eq!(store.read().await.unwrap(), Some(refreshed.clone()));
        // An older file is a leftover, not an instruction.
        write_t3_credential_at(&directory, &original).unwrap();
        assert!(store.load().await.is_err());
        assert_eq!(store.read().await.unwrap(), Some(refreshed));
        assert!(directory.join(T3_CREDENTIAL_FILE).exists());
    }

    #[tokio::test]
    async fn an_import_cannot_replace_a_concurrent_credential_save() {
        let (directory, _db, store) = fixture();
        let legacy = credential();
        write_t3_credential_at(&directory, &legacy).unwrap();
        // The import observed an empty store before another writer saved.
        assert_eq!(store.read().await.unwrap(), None);
        let mut concurrent = legacy.clone();
        concurrent.token = "synthetic-concurrent-save".into();
        store.write(&concurrent).await.unwrap();
        store.import_if_empty(&legacy).await.unwrap();
        assert_eq!(store.read().await.unwrap(), Some(concurrent));
        assert!(store.load().await.is_err());
        assert_eq!(
            super::super::read_t3_credential_at(&directory).unwrap(),
            Some(legacy)
        );
    }

    #[tokio::test]
    async fn failed_store_and_corrupt_legacy_never_erase_the_source() {
        let (directory, db, store) = fixture();
        let original = credential();
        write_t3_credential_at(&directory, &original).unwrap();
        let path = directory.join(T3_CREDENTIAL_FILE);
        let before = std::fs::read(&path).unwrap();
        db.call(|db| {
            db.execute_batch("DROP TABLE t3_credentials")?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(store.load().await.is_err());
        assert!(store.save(&original).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let (directory, _db, store) = fixture();
        let path = directory.join(T3_CREDENTIAL_FILE);
        std::fs::write(&path, b"incomplete JSON").unwrap();
        assert!(store.load().await.is_err());
        assert_eq!(store.read().await.unwrap(), None);
        assert_eq!(std::fs::read(path).unwrap(), b"incomplete JSON");
    }
}
