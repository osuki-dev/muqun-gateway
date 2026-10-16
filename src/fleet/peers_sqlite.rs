//! SQLite adapter for [`PeerStore`](super::peers::PeerStore).

use std::sync::Arc;

use rusqlite::{params, Row};
use serde_json::Value;

use super::peers::{Peer, PeerStore};
use crate::platform::store::sqlite::SqliteStore;
use crate::tasks::ports::TaskFuture;

pub(crate) struct SqlitePeerStore {
    db: Arc<SqliteStore>,
}

impl SqlitePeerStore {
    pub(crate) fn new(db: Arc<SqliteStore>) -> Self {
        Self { db }
    }
}

const COLUMNS: &str = "peer_id, name, url, token, device_id, transport_key, added_ms, last_seen_ms, capabilities, manifest, server_id, unauthorized, transport_min_version";

impl PeerStore for SqlitePeerStore {
    fn put<'a>(&'a self, peer: &'a Peer) -> TaskFuture<'a, ()> {
        let peer = peer.clone();
        Box::pin(self.db.call(move |c| {
            let capabilities = serde_json::to_string(&peer.capabilities)?;
            let manifest = peer.manifest.as_ref().map(Value::to_string);
            c.execute(
                &format!(
                    "INSERT INTO peers({COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                     ON CONFLICT(peer_id) DO UPDATE SET name = excluded.name, url = excluded.url,
                       token = excluded.token, device_id = excluded.device_id,
                       transport_key = excluded.transport_key, added_ms = excluded.added_ms,
                       last_seen_ms = excluded.last_seen_ms, capabilities = excluded.capabilities,
                       manifest = excluded.manifest, server_id = excluded.server_id, unauthorized = excluded.unauthorized,
                       transport_min_version = MAX(peers.transport_min_version, excluded.transport_min_version)"
                ),
                params![
                    peer.peer_id,
                    peer.name,
                    peer.url,
                    peer.token,
                    peer.device_id,
                    peer.transport_key,
                    peer.added_ms as i64,
                    peer.last_seen_ms.map(|ms| ms as i64),
                    capabilities,
                    manifest,
                    peer.server_id,
                    peer.unauthorized,
                    peer.transport_min_version,
                ],
            )?;
            Ok(())
        }))
    }

    fn list<'a>(&'a self) -> TaskFuture<'a, Vec<Peer>> {
        Box::pin(self.db.call(move |c| {
            let mut statement = c.prepare(&format!(
                "SELECT {COLUMNS} FROM peers ORDER BY name COLLATE NOCASE"
            ))?;
            let rows = statement.query_map([], read_row)?;
            rows.map(|row| row?).collect()
        }))
    }

    fn remove<'a>(&'a self, peer_id: &'a str) -> TaskFuture<'a, bool> {
        let peer_id = peer_id.to_owned();
        Box::pin(
            self.db.call(move |c| {
                Ok(c.execute("DELETE FROM peers WHERE peer_id = ?1", [peer_id])? > 0)
            }),
        )
    }

    fn edit<'a>(
        &'a self,
        peer_id: &'a str,
        name: Option<String>,
        url: Option<String>,
    ) -> TaskFuture<'a, bool> {
        let peer_id = peer_id.to_owned();
        Box::pin(self.db.call(move |c| {
            Ok(c.execute(
                "UPDATE peers SET name = COALESCE(?2, name), url = COALESCE(?3, url)
                 WHERE peer_id = ?1",
                params![peer_id, name, url],
            )? > 0)
        }))
    }

    fn pin_transport_version<'a>(&'a self, peer_id: &'a str, version: u8) -> TaskFuture<'a, u8> {
        let peer_id = peer_id.to_owned();
        Box::pin(self.db.call(move |c| {
            anyhow::ensure!(
                (1..=2).contains(&version),
                "unsupported peer transport version"
            );
            Ok(c.query_row(
                "UPDATE peers SET transport_min_version = MAX(transport_min_version, ?2)
                 WHERE peer_id = ?1 RETURNING transport_min_version",
                params![peer_id, version],
                |row| row.get(0),
            )?)
        }))
    }

    fn set_unauthorized<'a>(&'a self, peer_id: &'a str) -> TaskFuture<'a, ()> {
        let peer_id = peer_id.to_owned();
        Box::pin(self.db.call(move |c| {
            c.execute(
                "UPDATE peers SET unauthorized = 1 WHERE peer_id = ?1",
                [peer_id],
            )?;
            Ok(())
        }))
    }

    fn seen<'a>(
        &'a self,
        peer_id: &'a str,
        at_ms: u64,
        capabilities: Option<Vec<String>>,
        manifest: Option<Value>,
    ) -> TaskFuture<'a, ()> {
        let peer_id = peer_id.to_owned();
        Box::pin(self.db.call(move |c| {
            let capabilities = capabilities
                .map(|list| serde_json::to_string(&list))
                .transpose()?;
            c.execute(
                "UPDATE peers SET last_seen_ms = ?2,
                   capabilities = COALESCE(?3, capabilities), manifest = COALESCE(?4, manifest)
                 WHERE peer_id = ?1",
                params![
                    peer_id,
                    at_ms as i64,
                    capabilities,
                    manifest.as_ref().map(Value::to_string)
                ],
            )?;
            Ok(())
        }))
    }
}

/// A row, with its JSON columns parsed outside rusqlite's error type.
fn read_row(row: &Row<'_>) -> rusqlite::Result<anyhow::Result<Peer>> {
    let added_ms: i64 = row.get(6)?;
    let last_seen_ms: Option<i64> = row.get(7)?;
    let capabilities: String = row.get(8)?;
    let manifest: Option<String> = row.get(9)?;
    let server_id: String = row.get(10)?;
    let unauthorized: bool = row.get(11)?;
    let transport_min_version: u8 = row.get(12)?;
    let (peer_id, name, url, token, device_id, transport_key) = (
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    );
    Ok((|| {
        Ok(Peer {
            peer_id,
            server_id,
            name,
            url,
            token,
            device_id,
            transport_key,
            transport_min_version,
            added_ms: added_ms as u64,
            last_seen_ms: last_seen_ms.map(|ms| ms as u64),
            capabilities: serde_json::from_str(&capabilities)?,
            manifest: manifest
                .map(|text| serde_json::from_str(&text))
                .transpose()?,
            unauthorized,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn outbound_transport_pin_survives_stale_writes_and_restart() {
        let path =
            crate::test_state_dir().join(format!("peer-pin-{}.sqlite3", uuid::Uuid::new_v4()));
        let db = SqliteStore::open(&path, None, &[super::super::peers::MIGRATION]).unwrap();
        let store = SqlitePeerStore::new(db.clone());
        let peer = Peer {
            peer_id: "peer-test".into(),
            server_id: "server-test".into(),
            name: "test".into(),
            url: "http://127.0.0.1:9".into(),
            token: "test-token".into(),
            device_id: "test-device".into(),
            transport_key: Some(crate::generate_token()),
            transport_min_version: 1,
            added_ms: 0,
            last_seen_ms: None,
            capabilities: vec![],
            manifest: None,
            unauthorized: false,
        };
        store.put(&peer).await.unwrap();
        assert_eq!(store.list().await.unwrap()[0].transport_min_version, 1);
        assert_eq!(
            store.pin_transport_version(&peer.peer_id, 2).await.unwrap(),
            2
        );
        assert_eq!(
            store.pin_transport_version(&peer.peer_id, 1).await.unwrap(),
            2
        );
        store.put(&peer).await.unwrap();
        store
            .seen(&peer.peer_id, 1, Some(vec![]), None)
            .await
            .unwrap();
        store
            .edit(&peer.peer_id, Some("renamed".into()), None)
            .await
            .unwrap();
        drop(store);
        drop(db);
        let db = SqliteStore::open(&path, None, &[super::super::peers::MIGRATION]).unwrap();
        let reloaded = SqlitePeerStore::new(db).list().await.unwrap();
        assert_eq!(reloaded[0].transport_min_version, 2);
        assert_eq!(reloaded[0].name, "renamed");
        assert!(reloaded[0].capabilities.is_empty());
    }
}
