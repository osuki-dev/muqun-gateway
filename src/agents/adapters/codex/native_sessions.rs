//! Durable evidence of explicit native actions, not runtime ownership.
//! Only the official server can grant ownership of a recorded thread again.
use std::path::Path;
use std::sync::Arc;

use crate::platform::store::sqlite::{Migration, SqliteStore};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "codex_native_sessions",
    steps: &["CREATE TABLE codex_native_sessions (
        codex_home TEXT NOT NULL,
        thread_id TEXT NOT NULL,
        PRIMARY KEY(codex_home, thread_id)
    );"],
};

pub(crate) struct NativeSessions {
    db: Arc<SqliteStore>,
}

impl NativeSessions {
    pub(crate) fn new(db: Arc<SqliteStore>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    fn key(home: &Path, id: &str) -> anyhow::Result<(String, String)> {
        anyhow::ensure!(home.is_absolute(), "Codex home must be absolute");
        uuid::Uuid::parse_str(id)?;
        // The caller supplies initialize's verified canonical home. Never
        // normalize a thread ID or substitute a workspace for that scope.
        Ok((
            home.to_str()
                .ok_or_else(|| anyhow::anyhow!("Codex home is not UTF-8"))?
                .into(),
            id.into(),
        ))
    }

    pub(crate) async fn contains(&self, home: &Path, id: &str) -> anyhow::Result<bool> {
        let (home, id) = Self::key(home, id)?;
        self.db.call(move |db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM codex_native_sessions WHERE codex_home=?1 AND thread_id=?2)",
                rusqlite::params![home, id], |row| row.get(0),
            )?)
        }).await
    }

    pub(crate) async fn record(&self, home: &Path, id: &str) -> anyhow::Result<()> {
        let (home, id) = Self::key(home, id)?;
        self.db
            .call(move |db| {
                db.execute(
                    "INSERT OR IGNORE INTO codex_native_sessions(codex_home,thread_id) VALUES(?1,?2)",
                    rusqlite::params![home, id],
                )?;
                Ok(())
            })
            .await
    }

    pub(crate) async fn remove(&self, home: &Path, id: &str) -> anyhow::Result<()> {
        let (home, id) = Self::key(home, id)?;
        self.db
            .call(move |db| {
                db.execute(
                    "DELETE FROM codex_native_sessions WHERE codex_home=?1 AND thread_id=?2",
                    rusqlite::params![home, id],
                )?;
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::daemon::tests::TestDirectory;

    #[tokio::test]
    async fn native_session_evidence_survives_reopen_and_is_scoped_to_home_and_exact_id() {
        let directory = TestDirectory::new();
        let path = directory.0.join("store.sqlite");
        let home = directory.0.join("home");
        let other_home = directory.0.join("other-home");
        let id = "00000000-0000-4000-8000-000000000001";
        let other_id = "00000000-0000-4000-8000-000000000002";
        let store = NativeSessions::new(SqliteStore::open(&path, None, &[MIGRATION]).unwrap());
        store.record(&home, id).await.unwrap();
        drop(store);
        let store = NativeSessions::new(SqliteStore::open(&path, None, &[MIGRATION]).unwrap());
        assert!(store.contains(&home, id).await.unwrap());
        assert!(!store.contains(&other_home, id).await.unwrap());
        assert!(!store.contains(&home, other_id).await.unwrap());
        assert!(store.record(&home, "not-an-id").await.is_err());
        store.remove(&other_home, id).await.unwrap();
        assert!(store.contains(&home, id).await.unwrap());
        store.remove(&home, id).await.unwrap();
        assert!(!store.contains(&home, id).await.unwrap());
    }
}
