//! T3 wire identities never escape into App routes. This is reversible URL-safe
//! encoding, not encryption or authorization; normal pairing checks still apply.
use crate::agents::ports::agent::AgentError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;
const PREFIX: &str = "t3id1_";

pub fn public_id(native: &str) -> String {
    if !native.starts_with(PREFIX)
        && native
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return native.into();
    }
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(native))
}

pub fn native_id(public: &str) -> Result<String, AgentError> {
    let Some(encoded) = public.strip_prefix(PREFIX) else {
        return Ok(public.into());
    };
    let invalid = || AgentError::RequestFailed("invalid T3 session identity".into());
    if encoded.len() > 10924 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid())?;
    let native = String::from_utf8(bytes).map_err(|_| invalid())?;
    if native.is_empty() || public_id(&native) != public {
        return Err(invalid());
    }
    Ok(native)
}

pub fn path_segment(public: &str) -> Result<String, AgentError> {
    let native = native_id(public)?;
    // Encode bytes, including literal percent sequences, exactly once.
    Ok(native
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect())
}

fn field(value: &mut Value, key: &str) {
    if let Some(Value::String(id)) = value.get_mut(key) {
        *id = public_id(id);
    }
}
fn thread(value: &mut Value) {
    field(value, "id");
    if let Some(lineage) = value.get_mut("lineage") {
        field(lineage, "parentThreadId");
    }
}
fn entity(value: &mut Value) {
    for key in [
        "threadId",
        "sourceThreadId",
        "childThreadId",
        "parentThreadId",
    ] {
        field(value, key);
    }
}

/// Only protocol identity fields are rewritten. Tool input/output, prompt text,
/// file content, provider session IDs, request IDs and history cursors are opaque.
pub fn expose(value: &mut Value) {
    entity(value);
    if let Some(t) = value.get_mut("thread") {
        thread(t);
    }
    if let Some(Value::Array(threads)) = value.get_mut("threads") {
        for t in threads {
            thread(t);
        }
    }
    if let Some(Value::Array(matches)) = value.get_mut("matches") {
        for m in matches {
            entity(m);
        }
    }
    if let Some(Value::Array(rows)) = value.get_mut("items") {
        for row in rows {
            entity(row);
            if let Some(item) = row.get_mut("item") {
                entity(item);
            }
        }
    }
    if let Some(p) = value.get_mut("projection") {
        if let Some(t) = p.get_mut("thread") {
            thread(t);
        }
        if let Some(object) = p.as_object_mut() {
            for rows in object.values_mut().filter_map(Value::as_array_mut) {
                for row in rows {
                    entity(row);
                    if let Some(item) = row.get_mut("item") {
                        entity(item);
                    }
                }
            }
        }
    }
    if let Some(snapshot) = value.get_mut("snapshot") {
        expose(snapshot);
    }
    if let Some(event) = value.get_mut("event") {
        entity(event);
        let thread_event = event["type"]
            .as_str()
            .is_some_and(|s| s.starts_with("thread."));
        if let Some(payload) = event.get_mut("payload") {
            entity(payload);
            if thread_event {
                thread(payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn opaque_ids_round_trip_without_a_process_local_map() {
        for id in [
            "thread",
            "thread:provider:task%3Achild%0Aitem",
            "t3id1_literal",
            "会话/child?#",
        ] {
            let public = public_id(id);
            assert!(public
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
            assert_eq!(native_id(&public).unwrap(), id);
            assert_eq!(public_id(id), public);
        }
        for bad in ["t3id1_", "t3id1_%", "t3id1_YQ", "t3id1__w"] {
            assert!(native_id(bad).is_err());
        }
    }
    #[test]
    fn protocol_references_match_without_rewriting_tool_payloads() {
        let id = "native%0Achild";
        let mut snapshot = json!({"projection":{"thread":{"id":id,"lineage":{"parentThreadId":id}},"visibleTurnItems":[{"sourceThreadId":id,"sourceItemId":"item","item":{"threadId":id,"childThreadId":id,"input":{"threadId":id},"result":id}}]}});
        expose(&mut snapshot);
        let public = public_id(id);
        assert_eq!(snapshot["projection"]["thread"]["id"], public);
        assert_eq!(
            snapshot["projection"]["thread"]["lineage"]["parentThreadId"],
            public
        );
        let row = &snapshot["projection"]["visibleTurnItems"][0];
        assert_eq!(row["sourceThreadId"], public);
        assert_eq!(row["item"]["childThreadId"], public);
        assert_eq!(row["item"]["input"]["threadId"], id);
        assert_eq!(row["item"]["result"], id);
        assert_eq!(path_segment(&public).unwrap(), "native%250Achild");
    }
}
