//! Durable records are change signals, not a second public transcript mapper.
//! Fetch signalled items through Codex's canonical, redacted item projection.
use serde_json::{json, Value};
use std::collections::HashSet;

use super::CodexDriver;
use crate::agents::domain::TimelineItem;
use crate::agents::ports::agent::AgentError;

const MAX_TARGETS: usize = 64;
const MAX_CALLS: usize = 4;
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ItemTarget {
    pub turn: String,
    pub item: String,
}
#[derive(Default)]
pub(crate) struct Signals {
    pub items: Vec<ItemTarget>,
    pub usage: Option<Value>,
}

fn invalid() -> AgentError {
    AgentError::Protocol("Codex persisted observation requires an explicit refresh".into())
}
fn identifier(value: &Value) -> Result<String, AgentError> {
    let id = value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .ok_or_else(invalid)?;
    Ok(id.into())
}

impl Signals {
    pub(crate) fn parse(thread: &str, records: Vec<Value>) -> Result<Self, AgentError> {
        let mut signals = Self::default();
        let mut seen = HashSet::new();
        for record in records {
            if record["type"] != "event_msg" {
                continue;
            }
            let event = &record["payload"];
            match event["type"].as_str() {
                Some("item_completed") => {
                    if event["thread_id"] != thread {
                        return Err(invalid());
                    }
                    let target = ItemTarget {
                        turn: identifier(&event["turn_id"])?,
                        item: identifier(&event["item"]["id"])?,
                    };
                    if seen.insert(target.clone()) {
                        signals.items.push(target);
                    }
                    if signals.items.len() > MAX_TARGETS {
                        return Err(invalid());
                    }
                }
                Some("exec_command_end") => {
                    let target = ItemTarget {
                        turn: identifier(&event["turn_id"])?,
                        item: identifier(&event["call_id"])?,
                    };
                    if seen.insert(target.clone()) {
                        signals.items.push(target);
                    }
                    if signals.items.len() > MAX_TARGETS {
                        return Err(invalid());
                    }
                }
                Some("token_count") => {
                    if let Some(usage) = token_usage(&event["info"]) {
                        signals.usage = Some(usage);
                    }
                }
                // Rollback changes the visible projection. Do not append old
                // rows or replace a reader's history automatically.
                Some("thread_rolled_back") => return Err(invalid()),
                _ => {}
            }
        }
        Ok(signals)
    }
}

pub(crate) fn token_usage(info: &Value) -> Option<Value> {
    fn counters(value: &Value) -> Option<Value> {
        Some(json!({
            "totalTokens":value["total_tokens"].as_u64()?,
            "inputTokens":value["input_tokens"].as_u64()?,
            "cachedInputTokens":value["cached_input_tokens"].as_u64()?,
            "cacheWriteInputTokens":value.get("cache_write_input_tokens").map(Value::as_u64).unwrap_or(Some(0))?,
            "outputTokens":value["output_tokens"].as_u64()?,
            "reasoningOutputTokens":value["reasoning_output_tokens"].as_u64()?
        }))
    }
    Some(
        json!({"total":counters(&info["total_token_usage"])? ,"last":counters(&info["last_token_usage"])? ,
        "modelContextWindow":info["model_context_window"].as_u64()}),
    )
}

/// Each append batch has a fixed RPC/byte budget. Only the requested durable
/// IDs are emitted; adjacent history is neither repeated nor replaced.
pub(crate) async fn canonical_items(
    driver: &CodexDriver,
    thread: &str,
    targets: &[ItemTarget],
) -> Result<Vec<TimelineItem>, AgentError> {
    if targets.len() > MAX_TARGETS {
        return Err(invalid());
    }
    let mut turns = Vec::new();
    for target in targets {
        if !turns.contains(&target.turn) {
            turns.push(target.turn.clone());
        }
    }
    let mut calls = 0;
    let mut bytes = 0usize;
    let mut fetched = Vec::new();
    for turn in turns {
        let mut wanted: HashSet<_> = targets
            .iter()
            .filter(|target| target.turn == turn)
            .map(|t| t.item.clone())
            .collect();
        let mut cursor: Option<String> = None;
        let mut cursors = HashSet::new();
        let mut rows = Vec::new();
        while !wanted.is_empty() {
            calls += 1;
            if calls > MAX_CALLS {
                return Err(invalid());
            }
            let mut params =
                json!({"threadId":thread,"turnId":turn,"limit":8,"sortDirection":"desc"});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let (page, size) = driver
                .client()
                .call_sized("thread/items/list", params)
                .await?;
            bytes = bytes
                .checked_add(size)
                .filter(|total| *total <= MAX_BYTES)
                .ok_or_else(invalid)?;
            let data = page["data"]
                .as_array()
                .filter(|rows| rows.len() <= 8)
                .ok_or_else(invalid)?;
            for row in data {
                if row["turnId"] != turn {
                    return Err(invalid());
                }
                let id = identifier(&row["item"]["id"])?;
                if wanted.remove(&id) {
                    rows.push(row.clone());
                }
            }
            if !wanted.is_empty() {
                let next = page["nextCursor"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 16_384)
                    .ok_or_else(invalid)?;
                if !cursors.insert(next.to_owned()) {
                    return Err(invalid());
                }
                cursor = Some(next.into());
            }
        }
        // Native page direction is descending; retain producer item order.
        rows.reverse();
        for row in rows {
            fetched.push((turn.clone(), row));
        }
    }
    let mut items = Vec::new();
    for (turn, row) in fetched {
        items.extend(driver.ordered_items(
            thread,
            &turn,
            &row["item"],
            row["startedAtMs"].as_u64().unwrap_or(0),
        )?);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::tests::Fixture;
    #[test]
    fn signals_never_project_raw_tools_or_actionable_questions() {
        let records = vec![
            json!({"type":"event_msg","payload":{"type":"request_user_input","questions":["private"]}}),
            json!({"type":"response_item","payload":{"type":"function_call","arguments":"private"}}),
            json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"t","turn_id":"u","item":{"id":"i","type":"CommandExecution","command":["private"]}}}),
        ];
        let signals = Signals::parse("t", records).unwrap();
        assert_eq!(
            signals.items,
            vec![ItemTarget {
                turn: "u".into(),
                item: "i".into()
            }]
        );
        assert!(signals.usage.is_none());
        assert!(Signals::parse("other",vec![json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"t","turn_id":"u","item":{"id":"i"}}})]).is_err());
    }
    #[tokio::test]
    async fn requested_items_use_native_redacted_projection_and_preserve_order() {
        let mut fixture=Fixture::new(|r| {
            assert_eq!(r["method"],"thread/items/list");assert_eq!(r["params"]["turnId"],"turn");assert_eq!(r["params"]["limit"],8);
            json!({"data":[
                {"turnId":"turn","item":{"id":"new","type":"agentMessage","text":"new"}},
                {"turnId":"turn","item":{"id":"command","type":"commandExecution","command":"[REDACTED]","status":"completed","aggregatedOutput":"safe"}},
                {"turnId":"turn","item":{"id":"unrequested","type":"agentMessage","text":"old"}}],"nextCursor":null})
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let items = canonical_items(
            &driver,
            "thread",
            &[
                ItemTarget {
                    turn: "turn".into(),
                    item: "command".into(),
                },
                ItemTarget {
                    turn: "turn".into(),
                    item: "new".into(),
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            matches!(&items[0].part,crate::agents::domain::AgentPart::Tool(tool) if tool.input["command"]=="[REDACTED]")
        );
        assert!(
            matches!(&items[1].part,crate::agents::domain::AgentPart::Text{text} if text=="new")
        );
    }
    #[test]
    fn usage_is_observed_not_estimated_and_keeps_latest_complete_record() {
        let counts = json!({"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":13});
        let record = json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":counts,"last_token_usage":counts,"model_context_window":100}}});
        let usage = Signals::parse("thread", vec![record])
            .unwrap()
            .usage
            .unwrap();
        assert_eq!(usage["total"]["totalTokens"], 13);
        assert_eq!(usage["modelContextWindow"], 100);
        assert!(token_usage(&json!({"total_token_usage":{"input_tokens":-1}})).is_none());
    }
}
