//! Cursor-owned history windows. A prepared read cannot consume a cursor until
//! the mirror accepts its rows; pending rows from a large native page are kept.

use super::*;

#[derive(Clone)]
pub(super) struct HistoryCursor {
    seed: Value,
    pending: Vec<Value>,
    anchor: Option<(String, String)>,
}

impl HistoryCursor {
    pub(super) fn retained_bytes(&self) -> usize {
        serde_json::to_vec(&self.seed)
            .map_or(usize::MAX, |bytes| bytes.len())
            .saturating_add(
                serde_json::to_vec(&self.pending).map_or(usize::MAX, |bytes| bytes.len()),
            )
    }

    pub(super) fn select(mut detail: Value, limit: usize) -> Result<(Self, Value), AgentError> {
        let ordered = super::super::v2::projection::chronological_rows(
            super::super::v2::projection::rows(&detail["projection"], "visibleTurnItems"),
        )?;
        detail["projection"]["visibleTurnItems"] = Value::Array(ordered);
        let mapped =
            crate::agents::adapters::t3::v2::mapper::timeline("", &detail["projection"], 0);
        let first = if limit > 0 && mapped.len() > limit {
            Some(mapped[mapped.len() - limit].id.as_str())
        } else {
            None
        };
        let rows = detail["projection"]["visibleTurnItems"]
            .as_array_mut()
            .expect("validated rows");
        let cut = first
            .and_then(|id| {
                rows.iter().position(|row| {
                    crate::agents::adapters::t3::v2::mapper::row_id(row).as_deref() == Some(id)
                })
            })
            .unwrap_or(0);
        let pending: Vec<_> = rows.drain(..cut).collect();
        let anchor = rows.first().map(|row| {
            (
                row["sourceThreadId"]
                    .as_str()
                    .expect("validated source")
                    .to_owned(),
                row["sourceItemId"]
                    .as_str()
                    .expect("validated source")
                    .to_owned(),
            )
        });
        let seed = json!({
            "snapshotSequence": detail["snapshotSequence"],
            "historyCursor": detail["historyCursor"],
            "hasMoreHistory": detail["hasMoreHistory"],
            "projection": {
                "thread": detail["projection"]["thread"],
                "runtimeRequests": detail["projection"]["runtimeRequests"],
                "runs": detail["projection"]["runs"],
                "providerSessions": detail["projection"]["providerSessions"],
                "visibleTurnItems": []
            }
        });
        if !pending.is_empty() {
            detail["hasMoreHistory"] = json!(true);
        }
        Ok((
            Self {
                seed,
                pending,
                anchor,
            },
            detail,
        ))
    }
}

impl T3Client {
    pub(super) async fn walk_history(
        &self,
        id: &str,
        mut detail: Value,
        limit: usize,
        mut bytes: usize,
    ) -> Result<Value, AgentError> {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let count = super::super::v2::mapper::timeline(id, &detail["projection"], 0).len();
            if (limit > 0 && count >= limit) || detail["hasMoreHistory"] != true {
                break;
            }
            let cursor = detail["historyCursor"]
                .as_str()
                .ok_or_else(|| AgentError::Protocol("history has no cursor".into()))?
                .to_string();
            if cursor.len() > 8192 || !seen.insert(cursor.clone()) {
                return Err(AgentError::Protocol(
                    "invalid or repeated history cursor".into(),
                ));
            }
            let mut url = reqwest::Url::parse(&format!(
                "{}{HTTP_THREAD_SNAPSHOT}/{id}/history",
                self.endpoint.url
            ))
            .map_err(|_| AgentError::Protocol("invalid endpoint".into()))?;
            url.query_pairs_mut().append_pair("cursor", &cursor);
            let path = format!("{}?{}", url.path(), url.query().unwrap_or(""));
            let Some((page, consumed)) = self.get_json_counted(path, MAX_HTTP_BODY - bytes).await?
            else {
                break;
            };
            bytes += consumed;
            super::super::v2::projection::merge_history(&mut detail, &page)?;
            if detail["hasMoreHistory"] == true
                && detail["historyCursor"]
                    .as_str()
                    .is_some_and(|next| seen.contains(next))
            {
                return Err(AgentError::Protocol(
                    "invalid or repeated history cursor".into(),
                ));
            }
        }
        if detail["hasMoreHistory"] == true
            && (limit == 0
                || super::super::v2::mapper::timeline(id, &detail["projection"], 0).is_empty())
        {
            return Err(AgentError::Protocol(
                "history exceeds read-work budget".into(),
            ));
        }
        Ok(detail)
    }

    pub(crate) async fn prepare_earlier_snapshot(
        &self,
        id: &str,
        limit: usize,
    ) -> Result<Option<PreparedT3Timeline>, AgentError> {
        let (generation, mut cursor, _reader, anchor_row) = {
            let orders = self
                .timeline_orders
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let state = orders.get(id).ok_or(AgentError::HistoryCursorLost)?;
            let cursor = state.history.clone().ok_or(AgentError::HistoryCursorLost)?;
            let anchor_row = cursor
                .anchor
                .as_ref()
                .and_then(|(thread, item)| state.order.canonical_row(thread, item));
            (
                state.generation.clone(),
                cursor,
                state.readers.clone(),
                anchor_row,
            )
        };
        if cursor.pending.is_empty() && cursor.seed["hasMoreHistory"] == false {
            return Ok(None);
        }
        if cursor.anchor.is_some() && anchor_row.is_none() {
            return Err(AgentError::HistoryCursorLost);
        }
        cursor.seed["projection"]["visibleTurnItems"] =
            json!(anchor_row.into_iter().collect::<Vec<_>>());
        let anchor = cursor.seed["projection"]["visibleTurnItems"]
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(super::super::v2::mapper::row_id);
        let anchor_rows =
            super::super::v2::mapper::timeline(id, &cursor.seed["projection"], 0).len();
        let mut detail = cursor.seed;
        if !cursor.pending.is_empty() {
            let page = json!({"snapshotSequence":detail["snapshotSequence"],"items":cursor.pending,
                "nextCursor":detail["historyCursor"],"hasMoreHistory":detail["hasMoreHistory"]});
            super::super::v2::projection::merge_history(&mut detail, &page)?;
        }
        let detail = self
            .walk_history(id, detail, limit.saturating_add(anchor_rows), 0)
            .await?;
        let (cursor, mut detail) =
            HistoryCursor::select(detail, limit.saturating_add(anchor_rows))?;
        // Reaching the oldest prefix is not complete coverage of newer rows.
        detail["gatewayHistoricalPrefix"] = json!(true);
        detail["gatewayHistoryAnchorId"] = json!(anchor);
        let mut prepared = self.prepare_order_snapshot(id, detail, &generation, false)?;
        let useful = super::super::v2::mapper::timeline(id, &prepared.detail["projection"], 0)
            .iter()
            .any(|row| Some(row.id.as_str()) != anchor.as_deref());
        if !useful && (!cursor.pending.is_empty() || cursor.seed["hasMoreHistory"] == true) {
            return Err(AgentError::NotAvailable(
                "T3 history page contains no visible rows yet".into(),
            ));
        }
        prepared.history = Some(cursor);
        Ok(Some(prepared))
    }
}
