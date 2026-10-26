//! Native queued runs and steering; a restart is never a steering fallback.
use super::projection::{active_root_run, has_active_run, rows, string};
use crate::agents::adapters::t3::client::ms_from_iso;
use crate::agents::ports::agent::AgentError;
use serde_json::{json, Value};

fn supports(projection: &Value, run: &Value, capability: &str) -> bool {
    let Some(instance) = string(run, "providerInstanceId").filter(|id| !id.is_empty()) else {
        return false;
    };
    let provider_thread = string(run, "providerThreadId").or_else(|| {
        rows(projection, "nodes")
            .iter()
            .find(|node| string(node, "id") == string(run, "rootNodeId"))
            .and_then(|node| string(node, "providerThreadId"))
    });
    let Some(thread) = rows(projection, "providerThreads")
        .iter()
        .find(|thread| string(thread, "id") == provider_thread && provider_thread.is_some())
    else {
        return false;
    };
    let Some(session_id) = string(thread, "providerSessionId") else {
        return false;
    };
    if projection
        .pointer("/thread/activeProviderThreadId")
        .and_then(Value::as_str)
        != provider_thread
    {
        return false;
    }
    rows(projection, "providerSessions").iter().any(|session| {
        string(session, "id") == Some(session_id)
            && string(session, "providerInstanceId") == Some(instance)
            && string(thread, "providerInstanceId") == Some(instance)
            && matches!(
                string(session, "status"),
                Some("ready" | "running" | "waiting")
            )
            && session
                .pointer(&format!("/capabilities/turns/{capability}"))
                .and_then(Value::as_bool)
                == Some(true)
    })
}

pub fn steer_target(projection: &Value) -> Result<&str, AgentError> {
    let run = active_root_run(projection)
        .ok_or_else(|| AgentError::RequestFailed("no active root run to steer".into()))?;
    if !supports(projection, run, "supportsActiveSteering") {
        return Err(AgentError::Unsupported(
            "T3 provider active steering".into(),
        ));
    }
    string(run, "id").ok_or_else(|| AgentError::Protocol("active run lacks identity".into()))
}

pub fn dispatch_mode(projection: &Value, delivery: Option<&str>) -> Result<Value, AgentError> {
    match delivery.filter(|value| !value.is_empty()) {
        Some("steer") => {
            if !has_active_run(projection) {
                return Ok(json!({"type":"start_immediately"}));
            }
            Ok(json!({"type":"steer_active","targetRunId":steer_target(projection)?}))
        }
        None | Some("queue") => {
            if !has_active_run(projection) {
                return Ok(json!({"type":"start_immediately"}));
            }
            let run = active_root_run(projection)
                .ok_or_else(|| AgentError::Unsupported("T3 provider queue readiness".into()))?;
            if !supports(projection, run, "supportsQueuedMessages") {
                return Err(AgentError::Unsupported(
                    "T3 provider queued messages".into(),
                ));
            }
            Ok(json!({"type":"queue_after_active"}))
        }
        Some(_) => Err(AgentError::InvalidRequest("unknown delivery mode".into())),
    }
}

pub fn queued_run<'a>(projection: &'a Value, id: &str) -> Result<&'a Value, AgentError> {
    rows(projection, "runs")
        .iter()
        .find(|run| {
            string(run, "id") == Some(id)
                && string(run, "status") == Some("queued")
                && string(run, "threadId")
                    == projection.pointer("/thread/id").and_then(Value::as_str)
        })
        .ok_or_else(|| AgentError::InboxItemNotFound(id.into()))
}

pub fn items(projection: &Value) -> Result<Vec<Value>, AgentError> {
    let session = projection
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| AgentError::Protocol("T3 inbox lacks thread identity".into()))?;
    let mut queued: Vec<_> = rows(projection, "runs")
        .iter()
        .filter(|run| {
            string(run, "status") == Some("queued") && string(run, "threadId") == Some(session)
        })
        .collect();
    queued.sort_by_key(|run| {
        (
            run["queuePosition"].as_u64().unwrap_or(u64::MAX),
            run["ordinal"].as_u64().unwrap_or(0),
        )
    });
    queued
        .into_iter()
        .map(|run| {
            let id = string(run, "id")
                .ok_or_else(|| AgentError::Protocol("queued run lacks identity".into()))?;
            let message = rows(projection, "messages")
                .iter()
                .find(|message| {
                    string(message, "id") == string(run, "userMessageId")
                        && string(message, "threadId") == Some(session)
                        && string(message, "role") == Some("user")
                })
                .ok_or_else(|| AgentError::Protocol("queued run lacks its user message".into()))?;
            Ok(
                json!({"id":id,"sessionID":session,"type":"user","delivery":"queue",
            "timeCreated":string(run,"requestedAt").and_then(ms_from_iso),
            "payload":{"text":message["text"],"files":message["attachments"]}}),
            )
        })
        .collect()
}
