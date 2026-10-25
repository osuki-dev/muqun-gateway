//! Reader-owned history boundaries. Tokens name rows, never advance a reader
//! implicitly, and are scoped to one session incarnation on this Gateway.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};

use super::{MemoryMirror, SessionState};
use crate::agents::{
    domain::{AgentSessionId, TimelineItem},
    ports::{agent::AgentError, mirror::AgentSessionSnapshot},
};

const WINDOW_ROWS: usize = 100;
const PAGE_ROWS: usize = 80;

#[derive(Serialize)]
pub(crate) struct HistoryWindow {
    #[serde(flatten)]
    pub snapshot: AgentSessionSnapshot,
    pub history_cursor: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct HistoryPage {
    pub items: Vec<TimelineItem>,
    pub history_cursor: Option<String>,
    pub exhausted: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    session: String,
    epoch: String,
    generation: u64,
    anchor: String,
}

impl SessionState {
    fn cursor(&self, item: &TimelineItem) -> String {
        let cursor = Cursor {
            session: self.info.asid.0.clone(),
            epoch: self.history_epoch.clone(),
            generation: self.history_removal_generation,
            anchor: item.id.clone(),
        };
        // This concrete struct contains only strings and an integer.
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).expect("history cursor is serializable"))
    }

    fn anchor(&self, encoded: &str) -> Result<&TimelineItem, AgentError> {
        if encoded.len() > 4096 {
            return Err(AgentError::HistoryCursorLost);
        }
        let cursor: Cursor = URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or(AgentError::HistoryCursorLost)?;
        if cursor.session != self.info.asid.0
            || cursor.epoch != self.history_epoch
            || cursor.generation != self.history_removal_generation
        {
            return Err(AgentError::HistoryCursorLost);
        }
        self.timeline
            .get(&cursor.anchor)
            .or_else(|| {
                self.history_frontier
                    .as_ref()
                    .filter(|row| row.id == cursor.anchor)
            })
            .ok_or(AgentError::HistoryCursorLost)
    }
}

impl MemoryMirror {
    pub(crate) async fn history_window(
        &self,
        asid: &AgentSessionId,
        cursor: Option<&str>,
    ) -> Result<HistoryWindow, AgentError> {
        let sessions = self.sessions.read().await;
        let state = sessions.get(asid).ok_or(AgentError::HistoryCursorLost)?;
        let rows: Vec<_> = match cursor {
            Some(cursor) => {
                let anchor = state.anchor(cursor)?;
                // A retained frontier permits paging, but cannot reconstruct a
                // resync whose rows have already left the cache.
                if !state.timeline.contains_key(&anchor.id) {
                    return Err(AgentError::HistoryCursorLost);
                }
                state.timeline.from(anchor).collect()
            }
            None => {
                let mut rows: Vec<_> = state.timeline.ordered().rev().take(WINDOW_ROWS).collect();
                rows.reverse();
                rows
            }
        };
        Ok(HistoryWindow {
            history_cursor: rows.first().map(|row| state.cursor(row)),
            snapshot: AgentSessionSnapshot {
                info: state.info.clone(),
                timeline: rows.iter().map(|row| (*row).clone()).collect(),
                permissions: state.permissions.values().cloned().collect(),
                forms: state.forms.values().cloned().collect(),
                inbox: state.inbox.clone(),
                seq: state.current_seq,
            },
        })
    }

    /// The caller holds the session history gate through the provider read.
    pub(crate) async fn history_boundary(
        &self,
        asid: &AgentSessionId,
        cursor: &str,
    ) -> Result<TimelineItem, AgentError> {
        let sessions = self.sessions.read().await;
        Ok(sessions
            .get(asid)
            .ok_or(AgentError::HistoryCursorLost)?
            .anchor(cursor)?
            .clone())
    }

    /// A provider may backfill a newly discovered row on either side of the
    /// reader boundary (for example when Codex establishes canonical ordering).
    /// Return every mapped source row: filtering it by the old boundary would
    /// permanently hide it on the first traversal. Only the cursor moves back.
    pub(crate) async fn fetched_history_page(
        &self,
        asid: &AgentSessionId,
        cursor: &str,
        boundary: &TimelineItem,
        mut items: Vec<TimelineItem>,
        exhausted: bool,
    ) -> Result<HistoryPage, AgentError> {
        let sessions = self.sessions.read().await;
        let state = sessions.get(asid).ok_or(AgentError::HistoryCursorLost)?;
        if state.cursor(boundary) != cursor {
            return Err(AgentError::HistoryCursorLost);
        }
        let boundary = state.timeline.get(&boundary.id).unwrap_or(boundary);
        items.sort_unstable_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        let next = items
            .iter()
            .find(|row| row.sort_key() < boundary.sort_key())
            .map(|row| state.cursor(row))
            .unwrap_or_else(|| cursor.to_owned());
        Ok(HistoryPage {
            history_cursor: Some(next),
            exhausted: exhausted && items.is_empty(),
            items,
        })
    }

    /// Only clone rows being returned. `fetched` also covers provider pages
    /// older than the bounded mirror: a single reader can page beyond its cap.
    pub(crate) async fn history_page(
        &self,
        asid: &AgentSessionId,
        cursor: &str,
        boundary: &TimelineItem,
        fetched: &[TimelineItem],
        exhausted: bool,
    ) -> Result<HistoryPage, AgentError> {
        let sessions = self.sessions.read().await;
        let state = sessions.get(asid).ok_or(AgentError::HistoryCursorLost)?;
        // A fetch can advance the retained frontier. Check the epoch/removal
        // boundary against a token for the old row without requiring retention.
        if state.cursor(boundary) != cursor {
            return Err(AgentError::HistoryCursorLost);
        }
        let boundary = state.timeline.get(&boundary.id).unwrap_or(boundary);
        let mut rows: Vec<_> = state
            .timeline
            .before(boundary)
            .rev()
            .take(PAGE_ROWS)
            .collect();
        rows.reverse();
        if !fetched.is_empty() {
            rows.extend(fetched.iter().filter(|row| {
                !state.timeline.contains_key(&row.id) && row.sort_key() < boundary.sort_key()
            }));
            // Only the bounded page and any uncached provider rows participate.
            rows.sort_unstable_by_key(|row| row.sort_key());
        }
        let rows = &rows[rows.len().saturating_sub(PAGE_ROWS)..];
        Ok(HistoryPage {
            history_cursor: rows
                .first()
                .map(|row| state.cursor(row))
                .or_else(|| Some(cursor.to_owned())),
            exhausted: exhausted && rows.is_empty(),
            items: rows.iter().map(|row| (*row).clone()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, TimelineRole};
    use crate::agents::ports::mirror::SessionMirrorPort;
    use std::collections::HashSet;

    fn row(n: usize) -> TimelineItem {
        TimelineItem {
            id: format!("item-{n:06}"),
            message_id: format!("message-{n:06}"),
            role: TimelineRole::Assistant,
            part: AgentPart::Text {
                text: "message".repeat(100),
            },
            seq: 0,
            updated_ms: 1,
            ordinal: 0,
            attachments: None,
            image_assets: None,
        }
    }

    async fn fixture(count: usize) -> (MemoryMirror, AgentSessionId) {
        let mirror = MemoryMirror::for_agent("codex");
        let id = AgentSessionId("session".into());
        mirror
            .update_session(crate::test_support::fake_session("session", 1))
            .await;
        mirror
            .upsert_timeline_items(&id, (0..count).map(row).collect())
            .await;
        mirror.mark_history_initialized(&id).await;
        (mirror, id)
    }

    #[tokio::test]
    async fn warm_entry_and_independent_readers_page_without_gaps_or_duplicates() {
        let (mirror, id) = fixture(1683).await;
        let entry = mirror.history_window(&id, None).await.unwrap();
        assert_eq!(entry.snapshot.timeline.len(), 100);
        assert_eq!(entry.snapshot.timeline[0].id, row(1583).id);
        let first_cursor = entry.history_cursor.unwrap();
        let mut cursor = first_cursor.clone();
        let mut seen: HashSet<_> = entry
            .snapshot
            .timeline
            .iter()
            .map(|r| r.id.clone())
            .collect();
        loop {
            let boundary = mirror.history_boundary(&id, &cursor).await.unwrap();
            let page = mirror
                .history_page(&id, &cursor, &boundary, &[], false)
                .await
                .unwrap();
            if page.items.is_empty() {
                break;
            }
            for item in &page.items {
                assert!(seen.insert(item.id.clone()), "duplicate row");
            }
            cursor = page.history_cursor.unwrap();
        }
        assert_eq!(seen.len(), 1683);
        // Repeating a request or a second reader's first pull does not consume it.
        let boundary = mirror.history_boundary(&id, &first_cursor).await.unwrap();
        let second = mirror
            .history_page(&id, &first_cursor, &boundary, &[], false)
            .await
            .unwrap();
        assert_eq!(second.items.len(), 80);
        assert_eq!(second.items[0].id, row(1503).id);
        mirror.upsert_timeline_items(&id, vec![row(1683)]).await;
        let resync = mirror
            .history_window(&id, second.history_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(resync.snapshot.timeline.first().unwrap().id, row(1503).id);
        assert_eq!(resync.snapshot.timeline.last().unwrap().id, row(1683).id);
        assert_eq!(
            mirror
                .history_window(&id, None)
                .await
                .unwrap()
                .snapshot
                .timeline
                .len(),
            100
        );
    }

    #[tokio::test]
    async fn cursors_reject_cross_session_gateway_recreation_removal_and_malformed_input() {
        let (mirror, id) = fixture(200).await;
        let cursor = mirror
            .history_window(&id, None)
            .await
            .unwrap()
            .history_cursor
            .unwrap();
        let (other_gateway, other_id) = fixture(200).await;
        assert!(other_gateway
            .history_boundary(&other_id, &cursor)
            .await
            .is_err());
        let other = AgentSessionId("other".into());
        mirror
            .update_session(crate::test_support::fake_session("other", 1))
            .await;
        assert!(mirror.history_boundary(&other, &cursor).await.is_err());
        for invalid in ["invalid", &"a".repeat(5000)] {
            assert!(mirror.history_boundary(&id, invalid).await.is_err());
        }
        mirror
            .sessions
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .history_removal_generation += 1;
        assert!(mirror.history_boundary(&id, &cursor).await.is_err());
        mirror.sessions.write().await.remove(&id);
        mirror
            .update_session(crate::test_support::fake_session("session", 1))
            .await;
        assert!(mirror.history_boundary(&id, &cursor).await.is_err());
    }

    #[tokio::test]
    async fn older_pages_do_not_become_live_output_and_can_continue_past_cache_capacity() {
        let mirror = MemoryMirror::for_agent("codex");
        let id = AgentSessionId("session".into());
        mirror
            .update_session(crate::test_support::fake_session("session", 1))
            .await;
        mirror
            .upsert_timeline_items(&id, (200..5200).map(row).collect())
            .await;
        let cursor = {
            let sessions = mirror.sessions.read().await;
            let state = sessions.get(&id).unwrap();
            state.cursor(&row(200))
        };
        let gate = mirror.history_gate(&id).await;
        let _read = gate.lock().await;
        let revision = mirror.history_read_revision(&id).await.unwrap();
        let boundary = mirror.history_boundary(&id, &cursor).await.unwrap();
        let fetched = mirror
            .accept_earlier_window_history(
                &id,
                &gate,
                revision,
                (120..200).map(row).collect(),
                None,
            )
            .await
            .unwrap();
        let page = mirror
            .history_page(&id, &cursor, &boundary, &fetched, false)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 80);
        assert_eq!(page.items[0].id, row(120).id);
        assert!(mirror
            .get_events_after(&id, revision.0)
            .await
            .unwrap()
            .is_empty());
        let delta = mirror.get_timeline_delta(&id, revision.0).await;
        assert!(delta.0.is_empty());
        let cursor = page.history_cursor.unwrap();
        let boundary = mirror.history_boundary(&id, &cursor).await.unwrap();
        let revision = mirror.history_read_revision(&id).await.unwrap();
        let fetched = mirror
            .accept_earlier_window_history(&id, &gate, revision, (40..120).map(row).collect(), None)
            .await
            .unwrap();
        let page = mirror
            .history_page(&id, &cursor, &boundary, &fetched, false)
            .await
            .unwrap();
        assert_eq!(page.items[0].id, row(40).id);
        assert_eq!(page.items.len(), 80);
        // An evicted older boundary must fail explicitly, never skip to the
        // shared provider cursor another reader has already advanced.
        assert!(mirror.history_boundary(&id, &cursor).await.is_err());
    }
    #[tokio::test]
    async fn provider_backfill_after_the_boundary_is_not_lost_on_first_traversal() {
        let (mirror, id) = fixture(200).await;
        let entry = mirror.history_window(&id, None).await.unwrap();
        let cursor = entry.history_cursor.unwrap();
        let boundary = mirror.history_boundary(&id, &cursor).await.unwrap();
        let mut discovered = row(150);
        discovered.id = "newly-discovered-canonical-row".into();
        let page = mirror
            .fetched_history_page(
                &id,
                &cursor,
                &boundary,
                vec![discovered.clone(), row(20)],
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            page.items.iter().map(|row| &row.id).collect::<Vec<_>>(),
            vec![&row(20).id, &discovered.id]
        );
        assert_ne!(page.history_cursor.as_ref().unwrap(), &cursor);
        let page = mirror
            .fetched_history_page(&id, &cursor, &boundary, vec![discovered], false)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.history_cursor.as_deref(), Some(cursor.as_str()));
        assert!(!page.exhausted);
    }
    #[tokio::test]
    async fn refresh_merges_newest_rows_and_rebases_retained_history_without_losing_reader_boundaries(
    ) {
        use crate::agents::ports::agent::HistoryCommit;
        let (mirror, id) = fixture(1000).await;
        let initial = mirror.history_window(&id, None).await.unwrap();
        let revision = mirror.snapshot_read_revision(&id).await;
        let mut fresh = row(950);
        fresh.part = AgentPart::Text {
            text: "refreshed".into(),
        };
        fresh.message_id = "canonical-newest".into();
        let snapshot = AgentSessionSnapshot {
            info: initial.snapshot.info.clone(),
            timeline: vec![fresh.clone()],
            permissions: vec![],
            forms: vec![],
            inbox: vec![],
            seq: 0,
        };
        let commit = HistoryCommit {
            order: [
                (row(10).message_id, "message-000011-before".into()),
                (fresh.message_id.clone(), "must-not-rebase-fresh".into()),
            ]
            .into(),
            fresh_ids: [fresh.id.clone()].into(),
            ..Default::default()
        };
        mirror
            .refresh_prepared_history(snapshot.clone(), revision, Some(Box::new(|| Ok(commit))))
            .await
            .unwrap()
            .unwrap();
        let all = mirror.get_snapshot(&id).await.unwrap();
        assert_eq!(all.timeline.len(), 1000);
        assert_eq!(
            all.timeline
                .iter()
                .find(|item| item.id == row(10).id)
                .unwrap()
                .message_id,
            "message-000011-before"
        );
        assert_eq!(
            all.timeline
                .iter()
                .find(|item| item.id == fresh.id)
                .unwrap(),
            &fresh
        );
        let resynced = mirror
            .history_window(&id, initial.history_cursor.as_deref())
            .await
            .unwrap();
        assert!(resynced
            .snapshot
            .timeline
            .iter()
            .any(|item| item.id == row(900).id));
        assert!(all
            .timeline
            .windows(2)
            .all(|pair| pair[0].sort_key() < pair[1].sort_key()));
        assert!(mirror
            .get_events_after(&id, initial.snapshot.seq)
            .await
            .is_none());
        assert!(mirror
            .get_events_after(&id, all.seq)
            .await
            .unwrap()
            .is_empty());

        // A stale read and a refused provider commit must not change either
        // retained history or the ordered index.
        let revision = mirror.snapshot_read_revision(&id).await;
        mirror
            .update_status(&id, crate::agents::domain::AgentSessionStatus::Busy, None)
            .await;
        assert!(mirror
            .refresh_prepared_history(
                snapshot.clone(),
                revision,
                Some(Box::new(|| panic!("stale read committed")))
            )
            .await
            .unwrap()
            .is_none());
        let revision = mirror.snapshot_read_revision(&id).await;
        assert!(mirror
            .refresh_prepared_history(
                snapshot,
                revision,
                Some(Box::new(|| Err(AgentError::HistoryCursorLost)))
            )
            .await
            .is_err());
        assert_eq!(
            mirror.get_snapshot(&id).await.unwrap().timeline,
            all.timeline
        );
    }
}
