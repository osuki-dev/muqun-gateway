//! Persistent, bounded SQLite cache of recent canonical session snapshots.
//!
//! Used exclusively as a fast read-only first-paint preview (`retained=true`)
//! after a Gateway restart or idle eviction, before background `prepare`/wake
//! brings the live agent back online. Works uniformly for every agent kind.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::OptionalExtension as _;

use crate::agents::domain::AgentSessionStatus;
use crate::agents::ports::mirror::AgentSessionSnapshot;
use crate::platform::store::sqlite::{Migration, SqliteStore};

const MAX_CACHED_SESSIONS: i64 = 200;
const MAX_PREVIEW_ITEMS: usize = 60;
const MAX_PREVIEW_BYTES: usize = 512 * 1024;

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "session_preview_cache",
    steps: &[r#"
CREATE TABLE IF NOT EXISTS session_preview_cache (
    asid TEXT PRIMARY KEY,
    updated_ms INTEGER NOT NULL,
    snapshot_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_session_preview_updated
    ON session_preview_cache(updated_ms DESC);
"#],
};

#[derive(Default)]
pub(crate) struct PreviewCache {
    /// Last `(seq, timeline_len)` queued for persistence per `asid` to avoid
    /// redundant SQLite writes when a reader polls an unchanged session.
    saved: Mutex<HashMap<String, (u64, usize)>>,
}

impl PreviewCache {
    /// Persist a bounded preview copy of `snapshot` asynchronously when it
    /// carries canonical timeline history newer than the last saved state.
    pub(crate) fn maybe_save(&self, db: &Arc<SqliteStore>, snapshot: &AgentSessionSnapshot) {
        if snapshot.timeline.is_empty() || snapshot.seq == 0 {
            return;
        }
        let asid = snapshot.info.asid.0.clone();
        if asid.is_empty() {
            return;
        }
        let stamp = (snapshot.seq, snapshot.timeline.len());
        {
            let mut saved = self.saved.lock().unwrap();
            if saved.get(&asid).copied() == Some(stamp) {
                return;
            }
            if saved.len() >= (MAX_CACHED_SESSIONS as usize) * 2 {
                saved.clear();
            }
            saved.insert(asid.clone(), stamp);
        }
        let Some(json) = encode_bounded_preview(snapshot, MAX_PREVIEW_ITEMS, MAX_PREVIEW_BYTES)
        else {
            return;
        };
        let updated_ms = crate::now_unix_ms() as i64;
        let db = Arc::clone(db);
        tokio::spawn(async move {
            let _ = db
                .call(move |c| {
                    c.execute(
                        "INSERT INTO session_preview_cache (asid, updated_ms, snapshot_json) \
                         VALUES (?1, ?2, ?3) \
                         ON CONFLICT(asid) DO UPDATE SET \
                             updated_ms = excluded.updated_ms, \
                             snapshot_json = excluded.snapshot_json",
                        rusqlite::params![asid, updated_ms, json],
                    )?;
                    c.execute(
                        "DELETE FROM session_preview_cache WHERE asid NOT IN (\
                             SELECT asid FROM session_preview_cache \
                             ORDER BY updated_ms DESC LIMIT ?1\
                         )",
                        [MAX_CACHED_SESSIONS],
                    )?;
                    Ok(())
                })
                .await;
        });
    }

    /// Read a cached preview snapshot for `asid` without touching any agent.
    pub(crate) async fn load(
        &self,
        db: &Arc<SqliteStore>,
        asid: &str,
    ) -> Option<AgentSessionSnapshot> {
        let key = asid.to_owned();
        let raw: Option<String> = db
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT snapshot_json FROM session_preview_cache WHERE asid = ?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?)
            })
            .await
            .ok()
            .flatten();
        let mut snapshot: AgentSessionSnapshot = serde_json::from_str(&raw?).ok()?;
        if snapshot.timeline.is_empty() {
            return None;
        }
        snapshot.seq = 0;
        snapshot.permissions.clear();
        snapshot.forms.clear();
        snapshot.inbox.clear();
        if matches!(
            snapshot.info.status,
            AgentSessionStatus::Busy | AgentSessionStatus::Retry
        ) {
            snapshot.info.status = AgentSessionStatus::Idle;
        }
        Some(snapshot)
    }

    pub(crate) async fn delete(&self, db: &Arc<SqliteStore>, asid: &str) {
        if let Ok(mut saved) = self.saved.lock() {
            saved.remove(asid);
        }
        let key = asid.to_owned();
        let _ = db
            .call(move |c| {
                c.execute("DELETE FROM session_preview_cache WHERE asid = ?1", [key])?;
                Ok(())
            })
            .await;
    }
}

fn encode_bounded_preview(
    snapshot: &AgentSessionSnapshot,
    max_items: usize,
    max_bytes: usize,
) -> Option<String> {
    let start = snapshot.timeline.len().saturating_sub(max_items);
    let mut preview = AgentSessionSnapshot {
        info: snapshot.info.clone(),
        timeline: snapshot.timeline[start..].to_vec(),
        permissions: Vec::new(),
        forms: Vec::new(),
        inbox: Vec::new(),
        seq: 0,
    };
    while !preview.timeline.is_empty() {
        let json = serde_json::to_string(&preview).ok()?;
        if json.len() <= max_bytes {
            return Some(json);
        }
        let drop_count = (preview.timeline.len() / 2).max(1);
        if drop_count >= preview.timeline.len() {
            return None;
        }
        preview.timeline.drain(..drop_count);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole};

    fn sample_snapshot(asid: &str, items: usize, body_len: usize) -> AgentSessionSnapshot {
        let mut info = crate::test_support::fake_session(asid, 1000);
        info.agent_id = "codex".into();
        info.title = "Cached session".into();
        info.status = AgentSessionStatus::Busy;
        AgentSessionSnapshot {
            info,
            timeline: (0..items)
                .map(|i| TimelineItem {
                    id: format!("item-{i}"),
                    message_id: format!("msg-{i}"),
                    role: TimelineRole::Assistant,
                    part: AgentPart::Text {
                        text: format!("row-{i}:{}", "a".repeat(body_len)),
                    },
                    seq: (i + 1) as u64,
                    updated_ms: 1000 + i as u64,
                    ordinal: i as u64,
                    generation: None,
                    origin: None,
                    attachments: None,
                    image_assets: None,
                })
                .collect(),
            permissions: Vec::new(),
            forms: Vec::new(),
            inbox: Vec::new(),
            seq: 42,
        }
    }

    #[tokio::test]
    async fn save_load_and_delete_round_trip() {
        let dir = crate::test_state_dir().join(format!("preview-cache-{}", uuid::Uuid::new_v4()));
        let db = SqliteStore::open(&dir.join("gateway.sqlite3"), None, &[MIGRATION]).unwrap();
        let cache = PreviewCache::default();
        let snap = sample_snapshot("s-1", 5, 16);

        cache.maybe_save(&db, &snap);
        // Wait briefly for the spawned write to complete.
        let mut loaded = None;
        for _ in 0..40 {
            if let Some(found) = cache.load(&db, "s-1").await {
                loaded = Some(found);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let loaded = loaded.expect("cached preview should load");
        assert_eq!(loaded.timeline.len(), 5);
        assert_eq!(loaded.seq, 0);
        assert_eq!(loaded.info.status, AgentSessionStatus::Idle);

        cache.delete(&db, "s-1").await;
        assert!(cache.load(&db, "s-1").await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_snapshot_trims_oldest_items_to_fit_budget() {
        let snap = sample_snapshot("s-big", 20, 400);
        let encoded = encode_bounded_preview(&snap, 20, 2048).expect("should trim to budget");
        assert!(encoded.len() <= 2048);
        let decoded: AgentSessionSnapshot = serde_json::from_str(&encoded).unwrap();
        assert!(!decoded.timeline.is_empty());
        assert_eq!(decoded.timeline.last().unwrap().id, "item-19");
    }
}
