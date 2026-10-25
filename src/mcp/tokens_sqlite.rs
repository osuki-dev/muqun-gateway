//! SQLite adapter for [`AssistantTokenStore`](super::tokens::AssistantTokenStore).

use std::sync::Arc;

use rusqlite::{params, OptionalExtension as _, Row};

use super::tokens::{
    AssistantSession, AssistantTokenStore, IssuedToken, Resolved, TokenBinding, TokenRow, NO_EXPIRY,
};
use crate::platform::store::sqlite::SqliteStore;
use crate::tasks::domain::now_ms;
use crate::tasks::ports::TaskFuture;

pub(crate) struct SqliteTokenStore {
    db: Arc<SqliteStore>,
}

impl SqliteTokenStore {
    pub(crate) fn new(db: Arc<SqliteStore>) -> Self {
        Self { db }
    }
}

/// Longer than any token `generate_token()` mints; a longer one is never
/// hashed, let alone looked up.
const MAX_TOKEN_LEN: usize = 256;
const LIVE: &str = "revoked_ms IS NULL AND expires_ms > ?";

impl AssistantTokenStore for SqliteTokenStore {
    fn issue<'a>(
        &'a self,
        binding: TokenBinding,
        label: String,
        ttl_ms: u64,
    ) -> TaskFuture<'a, IssuedToken> {
        Box::pin(self.db.call(move |c| {
            let tx = c.transaction()?;
            if let TokenBinding::Runtime { agent_id } = &binding {
                anyhow::ensure!(crate::tasks::store_sqlite::runtime_registration_allowed(&tx, agent_id)?, "runtime_scope_conflict: retained delegated sessions require isolated credentials");
            }
            let token = crate::generate_token();
            let issued_ms = now_ms();
            let stored_expiry = issued_ms.saturating_add(ttl_ms).min(NO_EXPIRY);
            let session = AssistantSession {
                token_id: uuid::Uuid::new_v4().to_string(),
                expires_ms: (stored_expiry < NO_EXPIRY).then_some(stored_expiry),
                binding,
                label,
            };
            tx.execute(
                "INSERT INTO assistant_tokens(id, token_hash, binding, owner_key, label, issued_ms, expires_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session.token_id,
                    crate::hash_token(&token),
                    serde_json::to_string(&session.binding)?,
                    session.binding.owner_key(),
                    session.label,
                    issued_ms as i64,
                    stored_expiry as i64,
                ],
            )?;
            tx.commit()?;
            Ok(IssuedToken { token, session })
        }))
    }

    fn resolve<'a>(&'a self, token: &'a str) -> TaskFuture<'a, Resolved> {
        if token.len() > MAX_TOKEN_LEN {
            return Box::pin(async { Ok(Resolved::Invalid) });
        }
        let token_hash = crate::hash_token(token);
        Box::pin(self.db.call(move |c| {
            let now = now_ms() as i64;
            let row = c
                .query_row(
                    "SELECT id, binding, label, issued_ms, expires_ms, last_used_ms, revoked_ms FROM assistant_tokens WHERE token_hash = ?",
                    params![token_hash],
                    read_row,
                )
                .optional()?;
            let Some(row) = row else {
                return Ok(Resolved::Invalid);
            };
            let row = row?;
            if row.revoked_ms.is_some() {
                return Ok(Resolved::Invalid);
            }
            if row.expires_ms.is_some_and(|expires| expires as i64 <= now) {
                return Ok(Resolved::Expired);
            }
            c.execute(
                "UPDATE assistant_tokens SET last_used_ms = ?1 WHERE id = ?2",
                params![now, row.id],
            )?;
            Ok(Resolved::Live(AssistantSession {
                token_id: row.id,
                binding: row.binding,
                label: row.label,
                expires_ms: row.expires_ms,
            }))
        }))
    }

    fn revoke<'a>(&'a self, token_id: &'a str) -> TaskFuture<'a, bool> {
        let token_id = token_id.to_string();
        Box::pin(self.db.call(move |c| {
            let now = now_ms() as i64;
            let changed = c.execute(
                &format!("UPDATE assistant_tokens SET revoked_ms = ?1 WHERE id = ?2 AND {LIVE}"),
                params![now, token_id, now],
            )?;
            Ok(changed > 0)
        }))
    }

    #[cfg(test)]
    fn revoke_binding<'a>(&'a self, binding: &'a TokenBinding) -> TaskFuture<'a, usize> {
        let owner_key = binding.owner_key();
        Box::pin(self.db.call(move |c| {
            let now = now_ms() as i64;
            Ok(c.execute(
                &format!(
                    "UPDATE assistant_tokens SET revoked_ms = ?1 WHERE owner_key = ?2 AND {LIVE}"
                ),
                params![now, owner_key, now],
            )?)
        }))
    }

    fn revoke_all<'a>(&'a self) -> TaskFuture<'a, usize> {
        Box::pin(self.db.call(move |c| {
            let now = now_ms() as i64;
            Ok(c.execute(
                &format!("UPDATE assistant_tokens SET revoked_ms = ?1 WHERE {LIVE}"),
                params![now, now],
            )?)
        }))
    }

    fn purge_dead<'a>(&'a self, before_ms: u64) -> TaskFuture<'a, usize> {
        Box::pin(self.db.call(move |c| {
            Ok(c.execute(
                "DELETE FROM assistant_tokens WHERE revoked_ms < ?1 OR expires_ms < ?1",
                [before_ms as i64],
            )?)
        }))
    }

    fn list<'a>(&'a self) -> TaskFuture<'a, Vec<TokenRow>> {
        Box::pin(self.db.call(move |c| {
            let mut statement = c.prepare(&format!(
                "SELECT id, binding, label, issued_ms, expires_ms, last_used_ms, revoked_ms FROM assistant_tokens WHERE {LIVE} ORDER BY issued_ms DESC"
            ))?;
            let rows = statement.query_map([now_ms() as i64], read_row)?;
            rows.map(|row| row?).collect()
        }))
    }
}

/// A row, with the binding's JSON parsed outside rusqlite's error type.
fn read_row(row: &Row<'_>) -> rusqlite::Result<anyhow::Result<TokenRow>> {
    let binding: String = row.get(1)?;
    let issued_ms: i64 = row.get(3)?;
    let expires_ms: i64 = row.get(4)?;
    let last_used_ms: Option<i64> = row.get(5)?;
    let revoked_ms: Option<i64> = row.get(6)?;
    let (id, label) = (row.get(0)?, row.get(2)?);
    Ok(serde_json::from_str(&binding)
        .map_err(anyhow::Error::from)
        .map(|binding| TokenRow {
            id,
            binding,
            label,
            issued_ms: issued_ms as u64,
            expires_ms: (expires_ms != NO_EXPIRY as i64).then_some(expires_ms as u64),
            last_used_ms: last_used_ms.map(|ms| ms as u64),
            revoked_ms: revoked_ms.map(|ms| ms as u64),
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tokens::MIGRATION;

    fn temp_store(name: &str) -> Arc<SqliteStore> {
        SqliteStore::open(
            &crate::test_state_dir().join(format!("{name}-{}.sqlite3", uuid::Uuid::new_v4())),
            None,
            &[crate::tasks::store_sqlite::MIGRATION, MIGRATION],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn issued_tokens_resolve_until_expiry_or_revocation() {
        let store = SqliteTokenStore::new(temp_store("tok"));
        let pane = TokenBinding::Pane {
            session_id: "s1".into(),
            pane_id: "%3".into(),
        };
        let issued = store
            .issue(pane.clone(), "claude".into(), 1_000)
            .await
            .unwrap();
        let session = store
            .resolve(&issued.token)
            .await
            .unwrap()
            .live()
            .expect("live");
        assert_eq!(session.binding, pane);
        assert_eq!(session.principal().id, "pane:s1\u{1f}%3");
        assert_eq!(store.resolve("nope").await.unwrap(), Resolved::Invalid);
        assert_eq!(store.revoke_binding(&pane).await.unwrap(), 1);
        assert_eq!(
            store.resolve(&issued.token).await.unwrap(),
            Resolved::Invalid,
            "revoked is invalid, not expired"
        );
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_expired_token_is_expired_not_invalid() {
        let store = SqliteTokenStore::new(temp_store("exp"));
        let pane = TokenBinding::Pane {
            session_id: "s1".into(),
            pane_id: "%3".into(),
        };
        let issued = store.issue(pane.clone(), "claude".into(), 0).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert_eq!(
            store.resolve(&issued.token).await.unwrap(),
            Resolved::Expired
        );
        assert!(store.list().await.unwrap().is_empty());

        // Purged once dead long enough: the expired and the revoked row go,
        // the live one stays.
        let live = store.issue(pane.clone(), "claude".into(), 60_000).await;
        let revoked = store.issue(pane, "claude".into(), 60_000).await.unwrap();
        store.revoke(&revoked.session.token_id).await.unwrap();
        assert_eq!(store.purge_dead(1).await.unwrap(), 0, "nothing that old");
        assert_eq!(store.purge_dead(now_ms() + 1).await.unwrap(), 2);
        assert_eq!(
            store.resolve(&issued.token).await.unwrap(),
            Resolved::Invalid
        );
        assert!(store
            .resolve(&live.unwrap().token)
            .await
            .unwrap()
            .live()
            .is_some());
    }

    #[tokio::test]
    async fn an_agent_token_never_expires() {
        let store = SqliteTokenStore::new(temp_store("agent"));
        let agent = TokenBinding::Agent {
            agent_id: "opencode".into(),
        };
        let issued = store.issue(agent, "oc".into(), NO_EXPIRY).await.unwrap();
        assert_eq!(issued.session.expires_ms, None);
        let live = store.resolve(&issued.token).await.unwrap().live();
        assert_eq!(live.map(|s| s.expires_ms), Some(None));
        assert_eq!(store.list().await.unwrap()[0].expires_ms, None);
    }
}
