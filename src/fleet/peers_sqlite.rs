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

const COLUMNS: &str = "peer_id, name, url, token, device_id, transport_key, added_ms, last_seen_ms, capabilities, manifest";

impl PeerStore for SqlitePeerStore {
    fn put<'a>(&'a self, peer: &'a Peer) -> TaskFuture<'a, ()> {
        let peer = peer.clone();
        Box::pin(self.db.call(move |c| {
            let capabilities = serde_json::to_string(&peer.capabilities)?;
            let manifest = peer.manifest.as_ref().map(Value::to_string);
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM peers WHERE name = ?1 COLLATE NOCASE AND peer_id <> ?2",
                params![peer.name, peer.peer_id],
            )?;
            tx.execute(
                &format!(
                    "INSERT INTO peers({COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT(peer_id) DO UPDATE SET name = excluded.name, url = excluded.url,
                       token = excluded.token, device_id = excluded.device_id,
                       transport_key = excluded.transport_key, added_ms = excluded.added_ms,
                       last_seen_ms = excluded.last_seen_ms, capabilities = excluded.capabilities,
                       manifest = excluded.manifest"
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
                ],
            )?;
            tx.commit()?;
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
            name,
            url,
            token,
            device_id,
            transport_key,
            added_ms: added_ms as u64,
            last_seen_ms: last_seen_ms.map(|ms| ms as u64),
            capabilities: serde_json::from_str(&capabilities)?,
            manifest: manifest
                .map(|text| serde_json::from_str(&text))
                .transpose()?,
        })
    })())
}
