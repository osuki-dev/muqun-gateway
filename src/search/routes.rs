//! `GET /api/search?q=&limit=&cursor=&agent_id=&directory=`: a paired
//! device's full-text search over this machine's agent sessions.

use std::collections::HashMap;

use axum::extract::{Query as Params, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use super::index::Filters;
use super::text::{Query, MAX_QUERY_CHARS};
use super::SearchRequest;
use crate::platform::http::{api_error, require_device};
use crate::{ApiResult, AppState};

pub(crate) const DEFAULT_LIMIT: usize = 20;
pub(crate) const MAX_LIMIT: usize = 50;
const MAX_DIRECTORY_BYTES: usize = 4096;
const MAX_AGENT_ID_CHARS: usize = 32;

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/search", get(search))
}

fn invalid(code: &str, message: &str) -> (StatusCode, Json<Value>) {
    api_error(StatusCode::BAD_REQUEST, code, message)
}

/// The request a query string makes, or the 400 it earns.
pub(crate) fn request(params: &HashMap<String, String>) -> ApiResult<SearchRequest> {
    let text = params.get("q").map(|q| q.trim()).unwrap_or_default();
    // Empty after trimming, or only quotes: `Query::parse` keeps no term.
    let query = Some(text)
        .filter(|t| t.chars().count() <= MAX_QUERY_CHARS && !t.chars().any(char::is_control))
        .and_then(Query::parse)
        .ok_or_else(|| {
            invalid(
                "invalid_query",
                "q must be 1 to 200 characters without control characters",
            )
        })?;
    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(limit) => limit
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| invalid("invalid_limit", "limit must be an integer from 1 to 50"))?,
    };
    let offset = match params.get("cursor") {
        None => 0,
        Some(cursor) => cursor
            .parse::<usize>()
            .ok()
            .filter(|_| cursor.len() <= 6 && cursor.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| {
                invalid(
                    "invalid_cursor",
                    "cursor must be a next_cursor this route returned",
                )
            })?,
    };
    let agent_id = match params.get("agent_id") {
        None => None,
        Some(agent)
            if !agent.is_empty()
                && agent.len() <= MAX_AGENT_ID_CHARS
                && agent.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                }) =>
        {
            Some(agent.clone())
        }
        Some(_) => {
            return Err(invalid(
                "invalid_agent_id",
                "agent_id must be an agent id such as codex or claude",
            ))
        }
    };
    let directory = match params.get("directory") {
        None => None,
        Some(directory)
            if directory.starts_with('/')
                && directory.len() <= MAX_DIRECTORY_BYTES
                && !directory.chars().any(char::is_control) =>
        {
            Some(directory.clone())
        }
        Some(_) => {
            return Err(invalid(
                "invalid_directory",
                "directory must be an absolute path of at most 4096 bytes",
            ))
        }
    };
    Ok(SearchRequest {
        query,
        limit,
        offset,
        filters: Filters {
            agent_id,
            directory,
        },
    })
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Params(params): Params<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let request = request(&params)?;
    let unavailable = || {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "search_unavailable",
            "The search index is not available on this computer",
        )
    };
    match state.search.search(request).await {
        Ok(Some(page)) => Ok(Json(json!({ "data": page }))),
        Ok(None) => Err(unavailable()),
        Err(error) => {
            tracing::warn!("search failed: {error}");
            Err(unavailable())
        }
    }
}

/// `/api/search` in the OpenAPI document (`platform::openapi`).
pub(crate) fn openapi() -> Value {
    let param = |name: &str, description: &str, schema: Value| json!({ "name": name, "in": "query", "required": name == "q", "description": description, "schema": schema });
    let hit = json!({
        "type": "object",
        "required": ["asid", "agent_id", "title", "item_id", "message_id", "role", "kind", "time_ms", "snippet", "highlights", "session_hits"],
        "properties": {
            "asid": { "type": "string" },
            "agent_id": { "type": "string" },
            "title": { "type": "string" },
            "directory": { "type": ["string", "null"] },
            "parent_asid": { "type": ["string", "null"], "description": "The parent session of a subagent's session" },
            "item_id": { "type": "string", "description": "The timeline row, as the session's timeline names it" },
            "message_id": { "type": "string" },
            "role": { "type": "string", "enum": ["user", "assistant"] },
            "kind": { "type": "string", "enum": ["text", "tool"], "description": "A prompt or reply, or a tool call's headline (command, path, pattern; never its output)" },
            "time_ms": { "type": "integer" },
            "snippet": { "type": "string", "description": "One line around the first match" },
            "highlights": { "type": "array", "items": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2 }, "description": "[start, end) of each match in snippet, in UTF-16 code units" },
            "session_hits": { "type": "integer", "description": "The session's matching rows among those ranked; at most 3 are listed" }
        }
    });
    json!({ "get": {
        "summary": "Full-text search over the content of this computer's agent sessions: prompts, replies and tool-call headlines. Requires the session_search_v1 capability and a device token",
        "description": "Every whitespace-separated term must match as a case-insensitive substring (any script; terms under three characters are matched by a scan). Results are ranked by relevance and recency, at most three per session, each session's hits together. The index is built in the background: indexing says how many known sessions are indexed. Only this computer is searched.",
        "parameters": [
            param("q", "1 to 200 characters, no control characters", json!({ "type": "string" })),
            param("limit", "Hits per page", json!({ "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT })),
            param("cursor", "next_cursor from the previous page", json!({ "type": "string" })),
            param("agent_id", "Only this agent's sessions", json!({ "type": "string" })),
            param("directory", "Only sessions in this absolute directory or below it", json!({ "type": "string" }))
        ],
        "responses": {
            "200": { "description": "A page of hits", "content": { "application/json": { "schema": {
                "type": "object",
                "required": ["data"],
                "properties": { "data": {
                    "type": "object",
                    "required": ["items", "next_cursor", "indexing"],
                    "properties": {
                        "items": { "type": "array", "items": hit },
                        "next_cursor": { "type": ["string", "null"] },
                        "indexing": { "type": "object", "required": ["done", "total"], "properties": {
                            "done": { "type": "integer" }, "total": { "type": "integer" }
                        }}
                    }
                }}
            }}}},
            "400": { "description": "invalid_query, invalid_limit, invalid_cursor, invalid_agent_id or invalid_directory" },
            "401": { "description": "Missing or invalid authorization" },
            "403": { "description": "Invalid token, or another gateway's token (peer_forbidden)" },
            "503": { "description": "search_unavailable: the index could not be opened" }
        }
    }})
}
