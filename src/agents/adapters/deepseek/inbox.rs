//! DeepSeek's durable `inbox` projection, mapped to the existing App contract.
use crate::agents::ports::agent::AgentError;
use serde_json::{json, Value};
use std::collections::HashSet;

pub fn items(session: &str, projections: &Value) -> Result<Vec<Value>, AgentError> {
    if projections.is_null() {
        return Err(AgentError::SessionNotFound(session.into()));
    }
    let inbox = projections.pointer("/values/inbox").ok_or_else(|| {
        AgentError::Unsupported("DeepSeek durable inbox projection unavailable".into())
    })?;
    let mut result = Vec::new();
    let mut ids = HashSet::new();
    for (target, delivery) in [("next-step", "steer"), ("next-turn", "queue")] {
        let messages = inbox
            .get(target)
            .and_then(Value::as_array)
            .ok_or_else(|| AgentError::Protocol("invalid DeepSeek inbox projection".into()))?;
        for message in messages {
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    AgentError::Protocol("DeepSeek inbox message lacks identity".into())
                })?;
            if !ids.insert(id) {
                return Err(AgentError::Protocol(
                    "duplicate DeepSeek inbox identity".into(),
                ));
            }
            let content = message
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| AgentError::Protocol("invalid DeepSeek inbox content".into()))?;
            let text = content
                .iter()
                .filter_map(|part| {
                    (part["type"] == "text")
                        .then(|| part["text"].as_str())
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n");
            result.push(
                json!({"id":id,"sessionID":session,"type":"user","delivery":delivery,
                "payload":{"text":text,"parts":content}}),
            );
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_lists_keep_identity_and_delivery_and_reject_invalid_snapshots() {
        let mut projection = json!({"values":{"inbox":{
            "next-step":[{"id":"s","content":[{"type":"text","text":"steer"}]}],
            "next-turn":[{"id":"q","content":[{"type":"text","text":"later"}]}]}}});
        let mapped = items("session", &projection).unwrap();
        assert_eq!(mapped[0]["delivery"], "steer");
        assert_eq!(mapped[1]["id"], "q");
        assert_eq!(mapped[1]["payload"]["text"], "later");
        projection["values"]["inbox"]["next-turn"][0]["id"] = json!("s");
        assert!(matches!(
            items("session", &projection),
            Err(AgentError::Protocol(_))
        ));
        assert!(matches!(
            items("session", &json!({"values":{}})),
            Err(AgentError::Unsupported(_))
        ));
    }
}
