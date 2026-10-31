//! One named Muqun entry per Codex host, shared by its threads. Configuration
//! contains only a loopback URL and a credential helper, never a bearer token.
use serde_json::{json, Value};

use super::CodexClient;
use crate::agents::ports::agent::AgentError;

fn refused() -> AgentError {
    AgentError::InvalidRequest(
        "The existing Muqun MCP configuration differs or is disabled; it was not changed".into(),
    )
}

/// Retain only the configuration needed for registration. Provider keys,
/// other MCP credentials and project configuration never leave the RPC reader.
pub(super) fn registration_read(value: &Value) -> Value {
    let mut config = json!({});
    if let Some(entry) = value.pointer("/config/mcp_servers/muqun") {
        let mut selected = serde_json::Map::new();
        for key in ["url", "http_headers_helper", "enabled", "command"] {
            if let Some(field) = entry.get(key) {
                selected.insert(key.into(), field.clone());
            }
        }
        config["mcp_servers"] = json!({"muqun":selected});
    }
    let layers: Vec<Value> = value.get("layers").and_then(Value::as_array).into_iter().flatten()
        .filter(|layer| layer.pointer("/name/type").and_then(Value::as_str) == Some("user"))
        .map(|layer| json!({
            "name": {"type":"user", "file":layer["name"]["file"], "profile":layer["name"]["profile"]},
            "version":layer["version"],
            "disabledReason": if layer.get("disabledReason").is_none_or(Value::is_null) { Value::Null } else { json!("disabled") },
        })).collect();
    json!({"config":config,"layers":layers})
}

pub(super) fn registration(read: &Value, entry: &Value) -> Result<Option<Value>, AgentError> {
    let config = read
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| AgentError::Protocol("Codex returned no configuration".into()))?;
    if let Some(existing) = config
        .get("mcp_servers")
        .and_then(|servers| servers.get("muqun"))
    {
        if existing.get("url") != entry.get("url")
            || existing.get("http_headers_helper") != entry.get("http_headers_helper")
            || existing.get("enabled") == Some(&Value::Bool(false))
            || existing
                .get("command")
                .is_some_and(|value| !value.is_null())
        {
            return Err(refused());
        }
        return Ok(None);
    }
    let layers = read
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Protocol("Codex returned no configuration layers".into()))?;
    let mut users = layers.iter().filter(|layer| {
        layer.pointer("/name/type").and_then(Value::as_str) == Some("user")
            && layer.pointer("/name/profile").is_none_or(Value::is_null)
            && layer.get("disabledReason").is_none_or(Value::is_null)
    });
    let layer = users.next().ok_or_else(refused)?;
    if users.next().is_some() {
        return Err(refused());
    }
    let file = layer
        .pointer("/name/file")
        .and_then(Value::as_str)
        .ok_or_else(refused)?;
    let version = layer
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(refused)?;
    if !std::path::Path::new(file).is_absolute() || version.is_empty() {
        return Err(refused());
    }
    Ok(Some(json!({
        "keyPath": "mcp_servers.muqun", "value": entry,
        "mergeStrategy": "replace", "filePath": file, "expectedVersion": version,
    })))
}

pub(super) async fn connected(
    client: &CodexClient,
    thread: Option<&str>,
) -> Result<bool, AgentError> {
    let reply = client.call("mcpServerStatus/list", json!({
        "threadId": thread, "serverName": "muqun", "detail": "toolsAndAuthOnly", "limit": 100,
    })).await?;
    let servers = reply
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Protocol("Codex returned no MCP inventory".into()))?;
    Ok(servers.iter().any(|server| {
        server.get("name").and_then(Value::as_str) == Some("muqun")
            && server
                .get("tools")
                .and_then(Value::as_object)
                .is_some_and(|tools| {
                    tools.contains_key("exec.run") && tools.contains_key("fleet.list")
                })
    }))
}

pub(super) async fn prepare(
    client: &CodexClient,
    entry: &Value,
    cwd: Option<&str>,
) -> Result<(), AgentError> {
    let read = client
        .call("config/read", json!({"includeLayers": true, "cwd": cwd}))
        .await?;
    let changed = if let Some(write) = registration(&read, entry)? {
        client.call("config/value/write", write).await?;
        true
    } else {
        false
    };
    if changed || !connected(client, None).await? {
        // Official reload updates loaded threads too. Never resume, restart,
        // or resubmit a turn to refresh its tools.
        client.call("config/mcpServer/reload", Value::Null).await?;
    }
    Ok(())
}

/// A read-only tool projection. It never allocates display ordinals, resumes a
/// thread or publishes the App's history cursor while authorizing an MCP call.
pub(super) async fn call_timeline(
    client: &CodexClient,
    id: &str,
    limit: usize,
    mode: Option<&str>,
) -> Result<Vec<crate::agents::domain::TimelineItem>, AgentError> {
    use super::budget::{next_cursor, Budget};
    use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole};
    let mut budget = Budget::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor = None;
    let mut rows = Vec::new();
    if limit == 0 || limit > 100 {
        return Err(AgentError::InvalidRequest("invalid MCP probe limit".into()));
    }
    match mode {
        Some("paginated") => {
            loop {
                let requested = limit - rows.len();
                let page = budget.call(client, "thread/items/list", json!({
                "threadId":id, "limit":requested, "sortDirection":"desc", "cursor":cursor
            })).await?;
                let data = page["data"]
                    .as_array()
                    .filter(|data| data.len() <= requested)
                    .ok_or_else(|| AgentError::Protocol("invalid MCP tool page".into()))?;
                cursor = next_cursor(&page, &mut seen)?;
                if data.is_empty() && cursor.is_some() {
                    return Err(AgentError::Protocol(
                        "empty continuing MCP tool page".into(),
                    ));
                }
                rows.extend(data.iter().cloned());
                if rows.len() >= limit || cursor.is_none() {
                    break;
                }
            }
        }
        Some("legacy") => {
            let page = budget
                .call(
                    client,
                    "thread/turns/list",
                    json!({
                        "threadId":id,"limit":1,"sortDirection":"desc","itemsView":"full"
                    }),
                )
                .await?;
            let turns = page["data"]
                .as_array()
                .filter(|turns| turns.len() <= 1)
                .ok_or_else(|| AgentError::Protocol("invalid MCP turn page".into()))?;
            if let Some(turn) = turns.first() {
                if turn["itemsView"] != "full" {
                    return Err(AgentError::Protocol("incomplete MCP turn page".into()));
                }
                let items = turn["items"]
                    .as_array()
                    .ok_or_else(|| AgentError::Protocol("invalid MCP turn items".into()))?;
                rows.extend(
                    items
                        .iter()
                        .rev()
                        .take(limit)
                        .map(|item| json!({"turnId":turn["id"],"item":item})),
                );
            }
        }
        _ => return Err(AgentError::Unsupported("Codex MCP history mode".into())),
    }
    let mut result = Vec::new();
    for row in rows {
        let turn = super::mapper::string(&row, "turnId")?;
        let item = &row["item"];
        let item_id = super::mapper::string(item, "id")?;
        if item["type"] != "mcpToolCall" {
            continue;
        }
        for (id, part) in super::items::parts(&turn, &item_id, item) {
            if matches!(&part, AgentPart::Tool(_)) {
                result.push(TimelineItem {
                    id,
                    message_id: turn.clone(),
                    role: TimelineRole::Assistant,
                    part,
                    seq: 0,
                    updated_ms: 0,
                    ordinal: 0,
                    attachments: None,
                    image_assets: None,
                    generation: None,
                });
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    #[tokio::test]
    async fn mcp_probe_projects_running_calls_without_display_history_state() {
        use crate::agents::domain::{AgentPart, ToolCallStatus};
        let mut fixture = Fixture::new(|request| {
            assert_eq!(request["method"], "thread/items/list");
            json!({"data":[{"turnId":"turn", "item":{
                "id":"call", "type":"mcpToolCall", "server":"muqun", "tool":"exec.run",
                "status":"inProgress", "arguments":{"machine":"machine-id","cwd":"/work","command":"pwd"}
            }}], "nextCursor":null})
        }).await;
        fixture.ready().await;
        let items = call_timeline(&fixture.client, "thread", 20, Some("paginated"))
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        let AgentPart::Tool(call) = &items[0].part else {
            panic!("tool evidence missing");
        };
        assert_eq!(call.name, "muqun:exec.run");
        assert_eq!(call.state, ToolCallStatus::Running);
        assert_eq!(call.input["machine"], "machine-id");
    }

    fn entry() -> Value {
        json!({"url":"http://127.0.0.1:1234/mcp","http_headers_helper":"synthetic-helper --agent-id codex"})
    }
    fn read() -> Value {
        json!({"config": {}, "layers":[{"name":{"type":"user","file":"/synthetic/config.toml"},"version":"version-a","config":{}}]})
    }

    #[test]
    #[cfg_attr(windows, ignore = "not yet ported to Windows; see docs/windows.md")]
    fn registration_is_scoped_version_checked_and_contains_no_bearer() {
        let write = registration(&read(), &entry()).unwrap().unwrap();
        assert_eq!(write["keyPath"], "mcp_servers.muqun");
        assert_eq!(write["expectedVersion"], "version-a");
        assert_eq!(write["filePath"], "/synthetic/config.toml");
        assert_eq!(write["value"], entry());
        assert!(!write.to_string().contains("Bearer"));
    }

    #[test]
    fn configuration_projection_discards_unrelated_secrets_and_layer_payloads() {
        let mut reply = read();
        reply["config"]["api_key"] = json!("synthetic-secret");
        reply["config"]["mcp_servers"] =
            json!({"muqun":entry(), "other":{"http_headers":{"Authorization":"synthetic-secret"}}});
        reply["layers"][0]["config"] = json!({"secret":"synthetic-secret"});
        let projected = registration_read(&reply);
        assert!(!projected.to_string().contains("synthetic-secret"));
        assert!(!projected.to_string().contains("other"));
        assert!(registration(&projected, &entry()).unwrap().is_none());
    }

    #[test]
    fn same_registration_is_reused_but_conflicts_and_disabled_entries_are_preserved() {
        let mut read = read();
        read["config"]["mcp_servers"] = json!({"muqun": entry(), "other": {"command":"other"}});
        assert!(registration(&read, &entry()).unwrap().is_none());
        read["config"]["mcp_servers"]["muqun"]["enabled"] = json!(false);
        assert!(registration(&read, &entry()).is_err());
        read["config"]["mcp_servers"]["muqun"] = json!({"url":"http://127.0.0.1:9999/mcp"});
        assert!(registration(&read, &entry()).is_err());
        assert_eq!(read["config"]["mcp_servers"]["other"]["command"], "other");
    }

    #[test]
    fn absent_or_ambiguous_user_layer_never_writes_without_a_version_fence() {
        let mut read = read();
        read["layers"][0]["version"] = Value::Null;
        assert!(registration(&read, &entry()).is_err());
        read["layers"] = json!([]);
        assert!(registration(&read, &entry()).is_err());
        assert!(registration(&json!({}), &entry()).is_err());
    }

    #[tokio::test]
    async fn connected_registration_is_not_rewritten_or_reloaded() {
        let mut fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "config/read" => json!({"config":{"mcp_servers":{"muqun":entry()}}}),
            "mcpServerStatus/list" => {
                json!({"data":[{"name":"muqun","tools":{"exec.run":{},"fleet.list":{}}}]})
            }
            method => panic!("unexpected mutation: {method}"),
        })
        .await;
        fixture.ready().await;
        prepare(&fixture.client, &entry(), None).await.unwrap();
        prepare(&fixture.client, &entry(), None).await.unwrap();
    }

    #[tokio::test]
    #[cfg_attr(windows, ignore = "not yet ported to Windows; see docs/windows.md")]
    async fn new_registration_reloads_without_restarting_or_sending_a_prompt() {
        let mut fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "config/read" => read(),
            "config/value/write" => {
                assert_eq!(request["params"]["expectedVersion"], "version-a");
                json!({})
            }
            "config/mcpServer/reload" => json!({}),
            method => panic!("unexpected call: {method}"),
        })
        .await;
        fixture.ready().await;
        prepare(&fixture.client, &entry(), None).await.unwrap();
    }

    #[tokio::test]
    async fn readiness_checks_the_selected_thread_and_requires_both_tools() {
        let mut fixture = Fixture::new(|request| {
            assert_eq!(request["method"], "mcpServerStatus/list");
            assert_eq!(request["params"]["threadId"], "thread-a");
            assert_eq!(request["params"]["serverName"], "muqun");
            json!({"data":[{"name":"muqun","tools":{"fleet.list":{}}}]})
        })
        .await;
        fixture.ready().await;
        assert!(!connected(&fixture.client, Some("thread-a")).await.unwrap());
    }
}
