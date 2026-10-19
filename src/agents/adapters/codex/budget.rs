use super::CodexClient;
use crate::agents::ports::agent::AgentError;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

pub const MAX_PAGES: usize = 128;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ROWS: usize = 20_000;

pub struct Budget {
    deadline: tokio::time::Instant,
    pages: usize,
    bytes: usize,
    byte_limit: usize,
    rows: usize,
}
fn exhausted() -> AgentError {
    AgentError::RequestFailed(
        "Codex traversal budget exhausted; no partial inventory/history returned".into(),
    )
}
impl Budget {
    pub fn new() -> Self {
        Self {
            deadline: tokio::time::Instant::now() + Duration::from_secs(60),
            pages: 0,
            bytes: 0,
            byte_limit: MAX_BYTES,
            rows: 0,
        }
    }
    pub async fn call(
        &mut self,
        client: &CodexClient,
        method: &str,
        params: Value,
    ) -> Result<Value, AgentError> {
        if self.pages >= MAX_PAGES || tokio::time::Instant::now() >= self.deadline {
            return Err(exhausted());
        }
        self.pages += 1;
        if client.is_shared() && super::client::image_history_request(method, &params) {
            self.byte_limit = super::client::MAX_HISTORY_WIRE_WORK_BYTES;
        }
        let (value, bytes) = tokio::time::timeout_at(
            self.deadline,
            client.call_sized_budgeted(
                method,
                params,
                self.deadline,
                self.byte_limit.saturating_sub(self.bytes),
            ),
        )
        .await
        .map_err(|_| exhausted())??;
        self.bytes = self.bytes.checked_add(bytes).ok_or_else(exhausted)?;
        if self.bytes > self.byte_limit {
            return Err(exhausted());
        }
        Ok(value)
    }
    pub fn rows(&mut self, rows: usize) -> Result<(), AgentError> {
        self.rows = self.rows.checked_add(rows).ok_or_else(exhausted)?;
        if self.rows > MAX_ROWS {
            return Err(exhausted());
        }
        Ok(())
    }
    pub fn can_read_complete_history_item(&self) -> bool {
        self.pages < MAX_PAGES
            && tokio::time::Instant::now() < self.deadline
            && super::client::MAX_HISTORY_WIRE_WORK_BYTES.saturating_sub(self.bytes)
                >= super::client::MAX_HISTORY_FRAME_BYTES
    }
}

pub fn next_cursor(
    value: &Value,
    seen: &mut HashSet<String>,
) -> Result<Option<String>, AgentError> {
    match value.get("nextCursor") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(cursor))
            if !cursor.is_empty()
                && cursor.len() <= 8192
                && !cursor.chars().any(char::is_control) =>
        {
            if !seen.insert(cursor.clone()) {
                return Err(AgentError::Protocol("Codex pagination cursor cycle".into()));
            }
            Ok(Some(cursor.clone()))
        }
        _ => Err(AgentError::Protocol(
            "invalid Codex pagination cursor".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::tests::Fixture;
    use serde_json::json;

    #[tokio::test]
    async fn codex_traversal_budgets_count_actual_body_bytes_and_fail_closed_at_deadline_and_row_cap(
    ) {
        let mut fixture=Fixture::new(|_|json!({"config":{"model_provider":"synthetic-provider","private_padding":"x".repeat(2048)}})).await;
        fixture.ready().await;
        let mut budget = Budget::new();
        let selection = budget
            .call(
                &fixture.client,
                "config/read",
                json!({"includeLayers":false}),
            )
            .await
            .unwrap();
        assert!(selection["config"].get("private_padding").is_none());
        assert!(
            budget.bytes > 2048,
            "wire bytes, not sanitized/reserialized selection size"
        );
        budget.bytes = MAX_BYTES - 100;
        assert!(budget
            .call(&fixture.client, "config/read", json!({}))
            .await
            .unwrap_err()
            .to_string()
            .contains("budget"));
        let mut budget = Budget::new();
        budget.deadline = tokio::time::Instant::now() - Duration::from_millis(1);
        assert!(budget
            .call(&fixture.client, "config/read", json!({}))
            .await
            .is_err());
        assert_eq!(budget.pages, 0);
        let mut budget = Budget::new();
        budget.rows(MAX_ROWS).unwrap();
        assert!(budget.rows(1).is_err());
        assert!(
            next_cursor(&json!({"nextCursor":null}), &mut HashSet::new())
                .unwrap()
                .is_none()
        );
        for cursor in [json!(123), json!(""), json!("synthetic\ninvalid")] {
            assert!(next_cursor(&json!({"nextCursor":cursor}), &mut HashSet::new()).is_err());
        }
        assert!(next_cursor(&json!({}), &mut HashSet::new()).is_err());
    }
}
