//! Per-message decode throughput from native persisted timestamps and usage.
//! Mirrors dsh-llm's first-token predicate and ui-chat's decode interval. Never
//! use sessionStats, Gateway arrival times, or an uncommitted live stream here.

use serde_json::Value;

use crate::agents::domain::MessageGeneration;

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

fn timestamp(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .filter(|time| (0..=MAX_SAFE_INTEGER).contains(time))
}

/// Read the original first-token time without expanding compact delta runs.
/// Validate every member, including the suffix after the first token, so a
/// malformed stream never produces an apparently trustworthy measurement.
fn first_token_time(stream: &Value, completed: i64) -> Option<i64> {
    let mut first = None;
    let mut previous = None;
    for record in stream.as_array()? {
        let (time, token) = match record.get("type")?.as_str()? {
            "chunk" => {
                let time = timestamp(record.get("time")?)?;
                let chunk = record.get("chunk")?.as_object()?;
                let token = match chunk.get("type")?.as_str()? {
                    "text-delta" | "reasoning-delta" => !chunk.get("text")?.as_str()?.is_empty(),
                    "tool-call-delta" => {
                        let args = chunk.get("argumentsDelta")?.as_str()?;
                        let named = match chunk.get("name") {
                            None => false,
                            Some(name) => {
                                name.as_str()?;
                                true
                            }
                        };
                        !args.is_empty() || named
                    }
                    "block-start" | "block-end" | "usage" | "finish" => false,
                    _ => return None,
                };
                (time, token.then_some(time))
            }
            kind @ ("text-chunks" | "reasoning-chunks" | "tool-call-chunks") => {
                let mut time = timestamp(record.get("time0")?)?;
                let index = record.get("index")?.as_u64()?;
                if index > MAX_SAFE_INTEGER as u64 {
                    return None;
                }
                let tool = kind == "tool-call-chunks";
                let named = if tool {
                    if record.get("id")?.as_str()?.is_empty() {
                        return None;
                    }
                    match record.get("name") {
                        None => false,
                        Some(name) => {
                            if name.as_str()?.is_empty() {
                                return None;
                            }
                            true
                        }
                    }
                } else {
                    false
                };
                let members = record
                    .get(if tool { "args" } else { "texts" })?
                    .as_array()?;
                let gaps = record.get("dt")?.as_array()?;
                if members.is_empty() || gaps.len() != members.len() - 1 {
                    return None;
                }
                let mut token = None;
                for (index, fragment) in members.iter().enumerate() {
                    if index > 0 {
                        let gap = timestamp(&gaps[index - 1])?;
                        time = time.checked_add(gap).filter(|v| *v <= MAX_SAFE_INTEGER)?;
                    }
                    if previous.is_some_and(|previous| time < previous) || time > completed {
                        return None;
                    }
                    previous = Some(time);
                    let text = fragment.as_str()?;
                    if token.is_none() && (named || !text.is_empty()) {
                        token = Some(time);
                    }
                }
                (time, token)
            }
            _ => return None,
        };
        if previous.is_some_and(|previous| time < previous) || time > completed {
            return None;
        }
        previous = Some(time);
        if first.is_none() {
            first = token;
        }
    }
    first
}

pub(super) fn message_generation(event: &Value) -> Option<MessageGeneration> {
    if event.get("type")?.as_str()? != "assistant/message" {
        return None;
    }
    let data = event.get("data")?;
    if data.get("interrupted").is_some_and(|value| value != false) {
        return None;
    }
    let output = data.pointer("/usage/outputTokens")?.as_f64()?;
    if !output.is_finite() || output <= 0.0 {
        return None;
    }
    let completed = timestamp(event.get("time")?)?;
    let first = first_token_time(data.get("stream")?, completed)?;
    let decode_ms = completed
        .checked_sub(first)
        .filter(|duration| *duration > 0)?;
    let tokens_per_second = output / (decode_ms as f64 / 1000.0);
    (tokens_per_second.is_finite() && tokens_per_second > 0.0)
        .then_some(MessageGeneration::Message { tokens_per_second })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message() -> Value {
        json!({"type":"assistant/message","time":1500,"data":{
            "message":{"content":[{"type":"text","text":"ok"}]},
            "usage":{"outputTokens":25},"interrupted":false,
            "stream":[{"type":"text-chunks","time0":900,"index":0,"dt":[100,100],"texts":["","o","k"]}]
        }})
    }

    #[test]
    fn measured_reply_uses_its_own_usage_and_first_nonempty_native_delta() {
        let expected = json!({"scope":"message","tokens_per_second":50.0});
        assert_eq!(
            serde_json::to_value(message_generation(&message()).unwrap()).unwrap(),
            expected
        );
        let rows = super::super::mapper::map_record(&message());
        assert_eq!(
            serde_json::to_value(rows[0].generation.as_ref().unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn optional_generation_preserves_legacy_wire_shape_and_only_marks_last_text() {
        let mut event = message();
        event["data"]["message"]["content"] = json!([
            {"type":"text","text":"first"},{"type":"reasoning","text":"thinking"},
            {"type":"text","text":"last"}
        ]);
        let rows = super::super::mapper::map_record(&event);
        assert!(rows[0].generation.is_none() && rows[1].generation.is_none());
        assert!(rows[2].generation.is_some());
        event["data"].as_object_mut().unwrap().remove("stream");
        let row = super::super::mapper::map_record(&event).remove(0);
        let wire = serde_json::to_value(&row).unwrap();
        assert!(wire.get("generation").is_none());
        let restored: crate::agents::domain::TimelineItem = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, row);
        assert!(serde_json::from_value::<MessageGeneration>(
            json!({"scope":"session","tokens_per_second":50})
        )
        .is_err());
    }

    #[test]
    fn native_reasoning_and_named_tool_deltas_count_as_first_tokens() {
        for record in [
            json!({"type":"reasoning-chunks","time0":1000,"index":0,"dt":[],"texts":["thinking"]}),
            json!({"type":"tool-call-chunks","time0":1000,"index":0,"dt":[],"id":"call","name":"read","args":[""]}),
            json!({"type":"chunk","time":1000,"chunk":{"type":"tool-call-delta","argumentsDelta":"","name":"read"}}),
            json!({"type":"chunk","time":1000,"chunk":{"type":"text-delta","text":" "}}),
        ] {
            let mut event = message();
            event["data"]["stream"] = json!([record]);
            assert!(matches!(
                message_generation(&event),
                Some(MessageGeneration::Message {
                    tokens_per_second: 50.0
                })
            ));
        }
    }

    #[test]
    fn unavailable_or_untrustworthy_measurements_are_absent() {
        for (pointer, value) in [
            ("/type", json!("assistant/attempt")),
            ("/data/interrupted", json!(true)),
            ("/data/usage", Value::Null),
            ("/data/usage/outputTokens", json!("25")),
            ("/data/usage/outputTokens", json!(-1)),
            ("/data/usage/outputTokens", json!(0)),
            ("/data/usage/outputTokens", json!(1e308)),
            ("/data/stream", Value::Null),
            ("/data/stream", json!([])),
            ("/data/stream/0/dt", json!([100])),
            ("/data/stream/0/dt", json!([100, -1])),
            ("/data/stream/0/texts", json!(["", "", ""])),
            ("/data/stream/0/time0", json!(1500)),
            ("/data/stream/0/time0", json!(9007199254740992u64)),
            ("/data/stream/0/type", json!("future-chunks")),
            ("/data/stream/0/texts", json!(["ok", null, "tail"])),
        ] {
            let mut event = message();
            *event
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("{pointer}")) = value;
            assert!(message_generation(&event).is_none(), "{pointer}: {event}");
        }
        let mut event = message();
        event["data"].as_object_mut().unwrap().remove("stream");
        event["data"]["sessionStats"] = json!({"decodeTokens":1000,"decodeMs":100});
        assert!(
            message_generation(&event).is_none(),
            "session totals are never a fallback"
        );
    }
}
