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

pub(crate) const PRESENTATION_MIGRATION: Migration = Migration {
    namespace: "codex_prompt_presentations",
    steps: &["CREATE TABLE codex_prompt_presentations (
        codex_home TEXT NOT NULL,
        thread_id TEXT NOT NULL,
        client_id TEXT NOT NULL,
        wire_text TEXT NOT NULL,
        display_text TEXT NOT NULL,
        PRIMARY KEY(codex_home, thread_id, client_id)
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

    pub(super) async fn record_presentation(
        &self,
        home: &Path,
        thread: &str,
        client: &str,
        value: super::presentation::Presentation,
    ) -> anyhow::Result<()> {
        let (home, thread) = Self::key(home, thread)?;
        uuid::Uuid::parse_str(client)?;
        let client = client.to_owned();
        anyhow::ensure!(
            value.wire.len().saturating_add(value.display.len()) <= super::presentation::MAX_BYTES,
            "prompt presentation budget exceeded"
        );
        self.db.call(move |db| {
            let tx = db.transaction()?;
            tx.execute("INSERT INTO codex_prompt_presentations(codex_home,thread_id,client_id,wire_text,display_text) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![home,thread,client,value.wire,value.display])?;
            let (count, bytes): (i64,i64) = tx.query_row("SELECT COUNT(*), COALESCE(SUM(length(CAST(wire_text AS BLOB))+length(CAST(display_text AS BLOB))),0) FROM codex_prompt_presentations", [], |r| Ok((r.get(0)?,r.get(1)?)))?;
            anyhow::ensure!(count <= super::presentation::MAX_RECORDS as i64 && bytes <= super::presentation::MAX_BYTES as i64, "prompt presentation retention exhausted");
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(super) async fn presentations(
        &self,
        home: &Path,
        thread: &str,
    ) -> anyhow::Result<Vec<(String, super::presentation::Presentation)>> {
        let (home, thread) = Self::key(home, thread)?;
        self.db.call(move |db| {
            let mut query = db.prepare("SELECT client_id,wire_text,display_text FROM codex_prompt_presentations WHERE codex_home=?1 AND thread_id=?2 ORDER BY rowid")?;
            let rows = query.query_map(rusqlite::params![home,thread], |r| Ok((r.get(0)?,super::presentation::Presentation { wire:r.get(1)?, display:r.get(2)? })))?;
            Ok(rows.collect::<Result<Vec<_>,_>>()?)
        }).await
    }

    pub(crate) async fn remove(&self, home: &Path, id: &str) -> anyhow::Result<()> {
        let (home, id) = Self::key(home, id)?;
        self.db
            .call(move |db| {
                let presentations: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='codex_prompt_presentations')", [], |row| row.get(0))?;
                if presentations {
                    db.execute("DELETE FROM codex_prompt_presentations WHERE codex_home=?1 AND thread_id=?2", rusqlite::params![home,id])?;
                }
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
    async fn presentation_migration_preserves_ownership_and_scopes_durable_message_metadata() {
        const OLD: Migration = Migration {
            namespace: "codex_native_sessions",
            steps: &[MIGRATION.steps[0]],
        };
        let directory = TestDirectory::new();
        let path = directory.0.join("presentation.sqlite");
        let home = directory.0.join("home");
        let other_home = directory.0.join("other-home");
        let thread = "00000000-0000-4000-8000-000000000001";
        let client = "00000000-0000-4000-8000-000000000002";
        let old = NativeSessions::new(SqliteStore::open(&path, None, &[OLD]).unwrap());
        old.record(&home, thread).await.unwrap();
        drop(old);
        let store = NativeSessions::new(
            SqliteStore::open(&path, None, &[MIGRATION, PRESENTATION_MIGRATION]).unwrap(),
        );
        assert!(store.contains(&home, thread).await.unwrap());
        store
            .record_presentation(
                &home,
                thread,
                client,
                super::super::presentation::Presentation {
                    wire: "wire with machine UUID".into(),
                    display: "original".into(),
                },
            )
            .await
            .unwrap();
        let unaccepted = "00000000-0000-4000-8000-000000000003";
        store
            .record_presentation(
                &home,
                unaccepted,
                client,
                super::super::presentation::Presentation {
                    wire: "unaccepted".into(),
                    display: "user".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            !store.contains(&home, unaccepted).await.unwrap(),
            "presentation is never native ownership evidence"
        );
        drop(store);
        let store = NativeSessions::new(
            SqliteStore::open(&path, None, &[MIGRATION, PRESENTATION_MIGRATION]).unwrap(),
        );
        let legacy_reader = NativeSessions::new(SqliteStore::open(&path, None, &[OLD]).unwrap());
        assert!(legacy_reader.contains(&home, thread).await.unwrap());
        drop(legacy_reader);
        let rows = store.presentations(&home, thread).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, client);
        assert_eq!(rows[0].1.display, "original");
        assert!(store
            .presentations(&other_home, thread)
            .await
            .unwrap()
            .is_empty());
        store.remove(&other_home, thread).await.unwrap();
        assert_eq!(store.presentations(&home, thread).await.unwrap().len(), 1);
        store.remove(&home, thread).await.unwrap();
        assert!(store.presentations(&home, thread).await.unwrap().is_empty());
    }

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
