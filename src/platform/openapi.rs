//! OpenAPI 3.1.0 specification and Scalar documentation HTML for Muqun Gateway.

use serde_json::{json, Value};

use crate::CONTENT_SCHEMA_VERSION;

fn agents_plane_schema() -> Value {
    json!({
        "type": "object",
        "required": ["supported", "agents", "features"],
        "properties": {
            "supported": { "type": "boolean", "description": "Whether any agent is connected or reachable" },
            "agents": {
                "type": "array",
                "description": "One entry per known agent, attached or not. `id` is the value to send as `agent_id`.",
                "items": {
                    "type": "object",
                    "required": ["id", "name", "kind", "status", "enabled", "features", "models", "modes"],
                    "properties": {
                        "id": { "type": "string", "enum": ["opencode", "deepseek"] },
                        "name": { "type": "string" },
                        "kind": { "type": "string" },
                        "status": { "type": "string", "enum": ["connected", "reachable", "offline", "disabled", "not_installed", "unconfigured"] },
                        "enabled": { "type": "boolean" },
                        "endpoint": { "type": "string", "description": "Omitted when unknown or for an unauthenticated caller" },
                        "version": { "type": "string", "description": "Omitted when unknown or for an unauthenticated caller" },
                        "features": {
                            "type": "object",
                            "properties": {
                                "streaming": { "type": "boolean" },
                                "reasoningEffort": { "type": "boolean" },
                                "modelSelection": { "type": "boolean" },
                                "toolApprovals": { "type": "boolean" },
                                "worktrees": { "type": "boolean" },
                                "revert": { "type": "boolean" },
                                "inbox": { "type": "boolean" }
                            }
                        },
                        "models": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["id", "name", "providerId", "supportsReasoning"],
                                "properties": {
                                    "id": { "type": "string" },
                                    "name": { "type": "string" },
                                    "providerId": { "type": "string" },
                                    "supportsReasoning": { "type": "boolean" },
                                    "reasoningEffortTiers": { "type": "array", "items": { "type": "string" } }
                                }
                            }
                        },
                        "modes": {
                            "type": "array",
                            "description": "The agent's modes (personas), such as OpenCode's build and plan.",
                            "items": {
                                "type": "object",
                                "required": ["id", "name"],
                                "properties": {
                                    "id": { "type": "string" },
                                    "name": { "type": "string" },
                                    "description": { "type": "string" }
                                }
                            }
                        }
                    }
                }
            },
            "features": {
                "type": "object",
                "properties": {
                    "multiAgent": { "type": "boolean" },
                    "catalogAggregation": { "type": "boolean" },
                    "sessionRouting": { "type": "boolean" }
                }
            }
        }
    })
}

pub fn openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Terminal Gateway API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Token-protected mobile API for controlling local terminal workspaces through a configured tmux or Herdr backend. Human-readable text is localized: send X-Muqun-Locale (or Accept-Language) with one of `en`, `zh-TW`, `zh-CN`, `ja`, `ko`, `de`, `fr`, `es`, `pt`, `ru`, `vi`, `th`. Error `code` values, decision names and other wire vocabulary are the same bytes in every locale."
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            }
        },
        "security": [{ "bearerAuth": [] }],
        "paths": {
            "/health": { "get": simple_endpoint("Gateway health") },
            "/api/capabilities": {
                "get": {
                    "summary": "Gateway Terminal and Agents dual-plane capability discovery",
                    "description": "Probes host multiplexers (tmux, herdr, conpty) and agents (OpenCode, DeepSeek). Returns dynamic capability matrix so mobile clients can render or hide tabs, reasoning effort selectors, and model pickers without App Store releases. Supports unauthenticated capability probing and sealed authenticated responses.",
                    "security": [],
                    "responses": capabilities_discovery_responses()
                }
            },
            "/api/discovery": {
                "get": {
                    "summary": "Gateway multi-plane discovery across the Terminal, Agents, and SSH planes",
                    "description": "Probes host multiplexers (tmux, herdr), agents (OpenCode, DeepSeek), and SSH access surfaces. Returns dynamic multi-plane capability matrix so clients can adapt their UI without hardcoded agent assumptions.",
                    "security": [],
                    "responses": capabilities_discovery_responses()
                }
            },
            "/api/meta": { "get": simple_endpoint("Gateway API, backend, and legacy compatibility metadata") },
            "/api/pair/request": {
                "post": {
                    "summary": "Request pairing from Muqun app",
                    "security": [],
                    "requestBody": json_body(object_schema(&[("request_id", "string"), ("device_name", "string")], &["request_id"])),
                    "responses": ok_response()
                }
            },
            "/api/pair/claim": {
                "post": {
                    "summary": "Claim pairing token with request id and confirmation code",
                    "security": [],
                    "requestBody": json_body(object_schema(&[("request_id", "string"), ("code", "string")], &["request_id", "code"])),
                    "responses": ok_response()
                }
            },
            "/api/pairings": { "get": simple_endpoint("List devices holding a gateway token") },
            "/api/pairings/{deviceId}": {
                "delete": {
                    "summary": "Revoke one device's gateway token",
                    "parameters": [path_param("deviceId")],
                    "responses": ok_response()
                }
            },
            "/api/devices/push-token": {
                "post": {
                    "summary": "Register this Muqun device's Expo push token",
                    "requestBody": json_body(object_schema(&[("token", "string"), ("platform", "string"), ("device_name", "string"), ("locale", "string")], &["token", "platform"])),
                    "responses": ok_response()
                },
                "delete": {
                    "summary": "Remove this Muqun device's Expo push token",
                    "requestBody": json_body(object_schema(&[("token", "string")], &["token"])),
                    "responses": ok_response()
                }
            },
            "/api/notifications/test": {
                "post": {
                    "summary": "Send a test push notification to registered Muqun devices",
                    "requestBody": json_body(object_schema(&[("title", "string"), ("body", "string"), ("data", "object")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/uploads": {
                "post": {
                    "summary": "Upload one image and get back a local path for an agent to read",
                    "description": "Images only: png, jpeg, gif, webp, and heic. The type is decided by sniffing the content, not by the filename, and everything else, including executables and scripts, is refused. The stored name is generated by the gateway; the returned name is only the sanitised client name. Uploads are deleted after 48 hours.",
                    "requestBody": multipart_file_body(),
                    "responses": upload_responses()
                }
            },
            "/api/uploads/{fileName}": {
                "get": {
                    "summary": "Stream one stored upload's bytes back, read-only",
                    "description": "The companion to the upload: `path` in the upload response is for the agent, which reads the file off this host, and `url` -- this route -- is for the app, which cannot. `fileName` is the generated stored name, a single path component of the alphabet the gateway itself mints; a separator, a traversal, a leading dot, a symlink, and an unknown name are all the same 404. The content type is sniffed from the bytes on every read, exactly as the upload sniffed it, so the stored extension never decides on its own. An upload past its 48-hour retention is a 404 whether or not the hourly sweep has already taken it. The response is never cached by an intermediary.",
                    "parameters": [path_param("fileName")],
                    "responses": upload_content_responses()
                }
            },
            "/api/sessions/{sessionId}/tabs/{tabId}/assets": {
                "get": {
                    "summary": "List files this tab produced recently, newest first",
                    "description": "Unified content model, schema version 1.0.0. The response is the versioned envelope: schema_version, capabilities, and data, with the assets under data.assets. Assets are fed by the Herdr worktree events the gateway subscribes to, and by an mtime scan of the tab's roots, which is what a cold start uses. The scan is shallow, budgeted, and skips dot directories, dependency directories, and build output. Scoped to tabId, not to the whole session or workspace: a tmux-backed session spans every project the developer has a window open on, and a tmux-backed workspace (a whole tmux session, commonly one long-running session with a window per project) spans every one of those projects too, so this answers only for the one tab the caller is looking at. A herdr-backed session's tabId is resolved to the workspace it belongs to instead, because herdr's own tabs sit inside one of its workspaces and that workspace must not narrow further.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("tabId"),
                        query_param("since", "Unix milliseconds, the same unit as modified_unix_ms; only files modified strictly after this are returned"),
                        query_param("limit", "How many assets to return, 1 to 200, default 50"),
                        query_param("kind", "Comma-separated allow-list of kinds -- image, markdown, text, pdf, binary -- filtered during the scan, so kind=image&limit=50 answers with the 50 newest images rather than the images among the 50 newest files. Absent or empty means every kind; a value outside the taxonomy matches nothing rather than erroring. The applied list is echoed back as data.kind"),
                        query_param("path", "Resolve one absolute path exactly, for a file path tapped in terminal output. Takes precedence over since and limit. Answers with one asset, or with none when the path does not canonicalize to a file inside this tab's roots -- a fenced-out path is a miss, not an error")
                    ],
                    "responses": assets_responses()
                }
            },
            "/api/assets/{assetId}/content": {
                "get": {
                    "summary": "Stream one asset's bytes, read-only",
                    "description": "The path must canonicalize to a regular file inside a workspace root the session currently has, so a symlink out of the root, a traversal, and an unknown id are all a 404. An asset indexed while its root was a live workspace outlives that workspace: when the roots no longer contain it, the entry's stored canonical path is replayed and served only if it canonicalizes back to itself byte-for-byte, so a symlink swapped into the old location is the same 404. A path that was never indexed has no entry to replay. The kind is sniffed again from the bytes on every read; a binary asset answers 415 with its metadata and no body. Assets larger than 10 MiB are refused with 413.",
                    "parameters": [path_param("assetId")],
                    "responses": asset_content_responses()
                }
            },
            "/api/sessions": { "get": simple_endpoint("List configured terminal backend sessions with a current connected flag; unreachable sessions remain configured but should not appear as switch targets") },
            "/api/sessions/{sessionId}/events": {
                "get": {
                    "summary": "Stream Herdr lifecycle events as Server-Sent Events",
                    "description": "Herdr events arrive as SSE `herdr` events. The gateway adds its own `asset.created` event, carrying one asset in the content-model envelope, when a Herdr worktree event reveals newly produced files. It obeys the same `types=` allow-list, under the name `asset.created`.",
                    "parameters": [path_param("sessionId")],
                    "responses": {
                        "200": { "description": "SSE stream of Herdr event JSON lines, plus gateway asset.created events" },
                        "401": { "description": "Missing or invalid authorization" },
                        "403": { "description": "Invalid token" }
                    }
                }
            },
            "/api/sessions/{sessionId}/agent-events": {
                "get": {
                    "summary": "Recent agent status transitions in this session, oldest first",
                    "description": "An in-memory ring of the last 200 status transitions per session, for a client building a digest of what happened while it was away. Every transition is recorded, not only the two that raise a push, so a pane that worked and then went idle is visible even though only one notification was sent. Each event carries seq, pane_id, agent, from, to and unix_ms -- ids and statuses, never terminal output or an agent's own wording. Poll with since=<the highest seq already seen>; the answer's next_since is what to send next time, and missed is true when the ring has already dropped something after that point. Nothing is persisted: a restarted gateway answers with an empty list, because it was not watching.",
                    "parameters": [
                        path_param("sessionId"),
                        query_param("since", "Only transitions with a higher seq are returned. Absent means everything still held")
                    ],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/snapshot": {
                "get": {
                    "summary": "Return the whole session in one call: workspaces, tabs, panes and agents",
                    "description": "One answer where a client would otherwise call /workspaces, /tabs, /panes and /agents, which is what a phone does to warm its home screen. `agents` is the same array `GET /api/sessions/{sessionId}/agents` returns, from the same backend call and with every field it has -- `instance_id`, the opaque identity an assignment is bound to, and `target`, the address it is sent to, included. It used to be derived from the panes instead and carried neither, so a client still had to call /agents; a pane id is not a substitute for either, because panes are reused and renumbered. Announced as the `session_snapshot` capability in /health, so a client can ask rather than probe for a 404.",
                    "parameters": [path_param("sessionId")],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/workspaces": {
                "get": session_endpoint("List workspaces"),
                "post": {
                    "summary": "Create a workspace",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(object_schema(&[("cwd", "string"), ("label", "string"), ("focus", "boolean")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/workspaces/{workspaceId}/focus": { "post": resource_endpoint("Focus a workspace", "workspaceId") },
            "/api/sessions/{sessionId}/workspaces/{workspaceId}": {
                "patch": {
                    "summary": "Rename a workspace",
                    "parameters": [path_param("sessionId"), path_param("workspaceId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a workspace", "workspaceId")
            },
            "/api/sessions/{sessionId}/tabs": {
                "get": session_endpoint("List tabs"),
                "post": {
                    "summary": "Create a tab",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(object_schema(&[("workspace_id", "string"), ("label", "string"), ("cwd", "string"), ("focus", "boolean")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/tabs/{tabId}/focus": { "post": resource_endpoint("Focus a tab", "tabId") },
            "/api/sessions/{sessionId}/tabs/{tabId}": {
                "patch": {
                    "summary": "Rename a tab",
                    "parameters": [path_param("sessionId"), path_param("tabId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a tab", "tabId")
            },
            "/api/sessions/{sessionId}/panes": {
                "get": {
                    "summary": "List panes",
                    "description": PANE_GEOMETRY_DOC,
                    "parameters": [path_param("sessionId")],
                    "responses": pane_geometry_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}": {
                "get": {
                    "summary": "Get a pane",
                    "description": PANE_GEOMETRY_DOC,
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": pane_geometry_response()
                },
                "patch": {
                    "summary": "Rename a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(object_schema(&[("label", "string")], &["label"])),
                    "responses": ok_response()
                },
                "delete": resource_endpoint("Close a pane", "paneId")
            },
            "/api/sessions/{sessionId}/panes/{paneId}/focus": { "post": resource_endpoint("Focus a pane", "paneId") },
            "/api/sessions/{sessionId}/panes/{paneId}/split": {
                "post": {
                    "summary": "Split a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["direction"],
                        "properties": {
                            "direction": { "type": "string", "enum": ["right", "down"] },
                            "ratio": { "type": "number" },
                            "command": { "type": "array", "items": { "type": "string" } },
                            "cwd": { "type": "string" },
                            "env": { "type": "object", "additionalProperties": true }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/zoom": {
                "post": {
                    "summary": "Acknowledge the legacy app viewport request without changing backend zoom",
                    "description": "Compatibility no-op. Released app builds call this when mounting a terminal; observing a pane must not mutate tmux or Herdr layout.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "properties": {
                            "mode": { "type": "string", "enum": ["on", "off", "toggle"], "default": "on" }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/keymaps": { "get": session_endpoint("Agent keymap coverage") },
            "/api/agents/catalog": {
                "get": {
                    "summary": "Agent kinds a task can be started with, and whether each one is installed",
                    "description": "Herdr resolves an agent kind to its canonical executable itself, so this is a picker feed rather than a launch table: `available` is a PATH probe for that executable on this host, and false is a hint, not a veto. Not session-scoped, because which binaries are installed is a property of the machine. `command` is remapped by `agent_commands` in the gateway's config.json for a host whose binary is named something else.",
                    "responses": agents_catalog_responses()
                }
            },
            "/api/sessions/{sessionId}/tasks": {
                "post": {
                    "summary": "Start a new task: a checkout, a workspace, an agent, and the first prompt",
                    "description": "With `branch_name`, the task gets its own git worktree; without it, the task runs in the repo as it stands. `repo_path` must be, or be inside, a workspace this session already has open -- anything else is 403, and a symlink out of one resolves to the outside path and fails there too. `branch_name` is held to letters, digits, dot, underscore, dash and slash, with `..`, a leading dash, and dot-leading segments refused, so it can only ever be a ref and never an argument. The agent is started, then the gateway waits for it to become interactive before the prompt is sent. Asking twice for the same branch reuses the existing checkout rather than making a second one.",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["repo_path", "agent"],
                        "properties": {
                            "repo_path": { "type": "string", "description": "Absolute path, inside a workspace this session has open" },
                            "branch_name": { "type": "string", "description": "Branch for a dedicated worktree; omit to work in the repo as it stands" },
                            "agent": { "type": "string", "description": "Agent kind from GET /api/agents/catalog" },
                            "prompt": { "type": "string", "description": "Sent once the agent is interactive" },
                            "workspace_label": { "type": "string" },
                            "agent_args": { "type": "array", "items": { "type": "string" } },
                            "startup_timeout_ms": { "type": "integer", "description": "How long to wait for the agent to become interactive, 3001 to 300000, default 30000" }
                        }
                    })),
                    "responses": task_responses()
                }
            },
            "/api/sessions/{sessionId}/spawn": {
                "post": {
                    "summary": "Start an agent in a new pane, without describing a repository",
                    "description": "The light half of task dispatch: run this agent, here. The agent must be one this gateway offers -- a Herdr kind or a profile in agents.json -- and cwd, when given, must be a directory this session already works in, exactly the fence repo_path is under; anything else is 403. With tab_id the pane is split off whatever that tab has focused, so a second agent lands beside the first; without it the agent gets a tab of its own. GET recent-cwds answers with the directories cwd will accept. The reply names the pane and says whether the agent came up and whether the prompt landed; a 207 means the pane exists and something after it did not, which is not the same as nothing having happened.",
                    "parameters": [path_param("sessionId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["agent"],
                        "properties": {
                            "agent": { "type": "string", "description": "Agent kind from GET /api/agents/catalog, or a profile named in agents.json" },
                            "cwd": { "type": "string", "description": "Absolute path, inside a workspace this session has open; omit to take the backend's default" },
                            "tab_id": { "type": "string", "description": "Split this tab's focused pane instead of opening a new tab" },
                            "prompt": { "type": "string", "description": "Sent once the agent is interactive" }
                        }
                    })),
                    "responses": task_responses()
                }
            },
            "/api/sessions/{sessionId}/recent-cwds": {
                "get": {
                    "summary": "The distinct working directories of this session's panes",
                    "description": "A picker for spawn, and deliberately not a directory browser: it answers with the cwds the session reports for the panes that exist right now, deduplicated and held to the same rule the asset scan uses, so the filesystem root and a bare home directory are not on it. Each entry carries path, name, the pane and workspace it came from, and git, which says whether the directory is a checkout. Nothing here can be used to walk the host.",
                    "parameters": [path_param("sessionId")],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/interrupt": {
                "post": {
                    "summary": "Stop whatever the agent in this pane is doing",
                    "description": "Sugar over send-keys, and worth an endpoint because the key is not the same on every agent: ctrl+c at a shell, esc in every agent this gateway has a profile for, and whatever agents.json says when it names one. A keystroke and nothing else -- no signal and no kill. The reply names the key that was sent, so a client can say what it did. The same key is on the pane's shortcuts response as `interrupt`.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/shortcuts": {
                "get": resource_endpoint("Key row and slash commands for a pane", "paneId")
            },
            "/api/sessions/{sessionId}/agents": { "get": session_endpoint("List agents") },
            "/api/sessions/{sessionId}/agents/{target}": { "get": resource_endpoint("Get an agent", "target") },
            "/api/sessions/{sessionId}/agents/{target}/focus": { "post": resource_endpoint("Focus an agent", "target") },
            "/api/sessions/{sessionId}/agents/{target}/send": {
                "post": {
                    "summary": "Send and submit text to an agent",
                    "parameters": [path_param("sessionId"), path_param("target")],
                    "requestBody": json_body(object_schema(&[("text", "string")], &["text"])),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/output": {
                "get": {
                    "summary": "Read pane output",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("source", "Pane read source, for example recent-unwrapped, recent, visible, or detection"),
                        query_param("lines", "Maximum line count"),
                        query_param("start", "First absolute line to read, 0 being the oldest the pane holds. Requires end."),
                        query_param("end", "One past the last absolute line to read. Requires start. A range wins over lines, and is clamped to 5000 lines and to what the pane holds rather than refused."),
                        query_param("format", "Output format: text or ansi")
                    ],
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/parts": {
                "get": {
                    "summary": "Read the pane's transcript normalized into content-model parts",
                    "description": "Unified content model, schema version 1.4.0. Same envelope as the asset endpoints: schema_version, capabilities, and data, with the ordered parts under data.parts. Two sources can answer, and data.pane.parts says which did. native: the agent runs a protocol the gateway was pointed at (opencode's server API today), so a tool's exit code, the patch an edit produced, the checklist a todo write submitted and any pending permission arrive as data; range then spans the adapter's own rendering, which is the parts' fallback_text joined by newlines. dictionary: the pane's recent-unwrapped text read through the marker table of whichever agent the session reports -- Claude Code, Qoder, Codex and opencode. text: no table covers this pane, so everything degraded to prose, which is an answer and not an error. Whichever source answered, every part carries fallback_text verbatim, so an unknown type still renders and a source that drifts loses structure and never loses content. data.pane.composer carries the slash commands this agent understands and whether @ file mentions make sense, and is absent entirely for an agent the gateway has no table for. The raw output endpoint is unchanged and remains the fallback path.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("lines", "How many lines of scrollback to normalize, 1 to 5000, default 400")
                    ],
                    "responses": parts_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/files": {
                "get": {
                    "summary": "Fuzzy path search inside the pane's workspace, for @ file mentions",
                    "description": "Answers paths only -- no contents, no sizes, no absolute paths. The only directory searched is the pane's own working directory as the session reports it, canonicalized, which is the same fence the asset API is gated on; a pane sitting at the filesystem root or straight in the home directory has no workspace and answers with an empty list rather than an error, so this cannot be used to probe the host. Symlinks are never followed, and dot, dependency and build directories are skipped, so nothing outside the root can be named. The query is a fuzzy subsequence match over the relative path, ranked so that the file name beats the directories above it; an empty query answers with the shallowest files, which is what a picker shows before anything is typed. kind is decided from the name alone because nothing is read -- the asset content endpoint sniffs the bytes again when a file is actually opened.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("query", "What the user typed after the @; empty or absent lists the shallowest files"),
                        query_param("limit", "How many matches to return, 1 to 50, default 20")
                    ],
                    "responses": file_search_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/context": {
                "get": {
                    "summary": "Where the pane is and what runs in it",
                    "description": "One read-only answer for the questions a client used to assemble from the pane, recent-cwds and the shortcuts: the working directory, whether it is inside the fence the asset and file routes use, the git checkout it belongs to (branch, upstream, ahead/behind, head, changed-file count) or null, and the agent running there (kind, status, foreground command, whether this gateway has a profile for it) or null for a plain shell. Facts about one pane, read on demand; capabilities stay on /api/health. Announced as pane_context.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": pane_context_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/git/status": {
                "get": {
                    "summary": "What changed in the pane's checkout",
                    "description": "git status --porcelain=v2 plus git diff --numstat HEAD, run read-only (--no-optional-locks, so the agent working in the checkout never loses index.lock) inside the checkout the pane's fenced working directory belongs to. One entry per changed file -- staged, unstaged and untracked together, which is what 'what did the agent change' means -- with line totals; binary files carry null totals. The list stops at 2000 files and says so with truncated; repo.changed_files is still the full count. A pane outside any checkout answers repo: null and an empty list, not an error. Announced as git_diff.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": git_status_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/git/diff": {
                "get": {
                    "summary": "One file's unified patch, one page at a time",
                    "description": "git diff -M -U<context> HEAD -- <path> [<old_path>], with an untracked file rendered against /dev/null. path (and old_path, for a rename) is the only client value that reaches git: relative, no .. components, not starting with -, placed after a literal --; an untracked path must additionally be a regular file (not a symlink) inside the checkout. from is a 0-based line offset into the whole patch and lines (at most 4000) the page size; a page is cut back to the nearest hunk or file boundary past its middle, so a page normally starts on @@ or diff --git; a single hunk longer than a page is cut raw and the next page continues it, so a reader carries the line counters from one page to the next. truncated says whether another page follows from end. binary is true for a change git prints no hunks for. 400 for a path that is not a relative path, 404 no_repository for a pane outside a checkout, 404 no_such_path for a path nothing in the checkout has, 504 when git takes more than five seconds.",
                    "parameters": [
                        path_param("sessionId"),
                        path_param("paneId"),
                        query_param("path", "The file, relative to the checkout's top level"),
                        query_param("old_path", "For a renamed or copied file, the path before, so git renders the rename rather than a new file"),
                        query_param("staged", "Absent: working tree against HEAD. true: index against HEAD. false: working tree against index"),
                        query_param("context", "Context lines per hunk, 0 to 25, default 3"),
                        query_param("from", "0-based line offset into the whole patch, default 0"),
                        query_param("lines", "Page size in patch lines, 1 to 4000, default 4000")
                    ],
                    "responses": git_diff_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/approval": {
                "get": {
                    "summary": "Read whether the pane is blocked on an approval, and what it asks",
                    "description": "Agents ask for permission by drawing a numbered menu and blocking. This reads that menu off the pane's visible screen: the question, the answers, which one the cursor is on, what each answer means (allow, allow_always, deny), and the lines the agent drew around the request. data.state is pending or idle, and data.approval is null when idle. The fingerprint identifies this question with these answers; send it back on POST and an approval that changed underneath is rejected rather than answered blind. Approvals are not parts: docs/content-model.md keeps the part set closed and gives approvals a part type only in v2, so until then they ride their own endpoint and their own SSE events (approval.pending, approval.resolved).",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "responses": approval_responses()
                },
                "post": {
                    "summary": "Answer the approval the pane is blocked on",
                    "description": "Answer by option number or by decision (allow, allow_always, deny); the gateway turns it into the keystrokes that agent's menu wants, and confirms with Enter only when the same menu is still standing afterwards. 409 when the pane is not waiting, or when the pending approval is not the one the fingerprint names. Raw send-keys remains the fallback for a menu no client understands.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "description": "Give option or decision; fingerprint is optional optimistic concurrency.",
                        "properties": {
                            "option": { "type": "integer", "description": "The option number the agent printed" },
                            "decision": { "type": "string", "enum": ["allow", "allow_always", "deny"] },
                            "fingerprint": { "type": "string" }
                        }
                    })),
                    "responses": approval_responses()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/send-text": {
                "post": {
                    "summary": "Send text to a pane, as a paste or as keystrokes",
                    "description": "`mode` says how the text should reach the program in the pane, because a terminal program acts on the difference. `paste` (the default, and what this route did before the field existed) wraps the text in bracketed-paste markers where the program asked for them: a multi-line composer message then arrives as one message instead of submitting at its first newline, and an agent that rewrites an attachment path into an image reference only does so inside its paste handler. `keys` delivers the bytes a keyboard would have produced, with no markers, which is what a virtual keyboard and an editor key row need -- nvim in Normal mode executes a typed `i` and inserts a pasted one. Omit the field and nothing changes; a value this gateway does not recognise is also treated as `paste` rather than refused, so a newer app and an older gateway keep working together.",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["text"],
                        "properties": {
                            "text": { "type": "string" },
                            "mode": {
                                "type": "string",
                                "enum": ["paste", "keys"],
                                "default": "paste",
                                "description": "How the text reaches the program: as a bracketed paste, or as keystrokes."
                            }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/sessions/{sessionId}/panes/{paneId}/send-keys": {
                "post": {
                    "summary": "Send key names to a pane",
                    "parameters": [path_param("sessionId"), path_param("paneId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["keys"],
                        "properties": { "keys": { "type": "array", "items": { "type": "string" } } }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/agent-status": {
                "get": {
                    "summary": "Get the status of one agent",
                    "description": "Returns availability, origin, endpoint, version and stream state of the agent named by agent_id, or of the primary agent when agent_id is absent. The response carries agent_id and kind. An unknown agent_id is 400 invalid_agent; a known one that is not attached is 503 agent_unavailable.",
                    "parameters": [query_param("agent_id", "The agent to describe: opencode or deepseek. Absent, the primary agent")],
                    "responses": ok_response()
                }
            },
            "/api/agent-catalog": {
                "get": {
                    "summary": "Global catalog of AI models and agent roles",
                    "description": "Lists available LLM models and agent modes, merged across every attached agent, or for one agent with agent_id.",
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions": {
                "get": {
                    "summary": "List all active AI agent sessions",
                    "description": "Returns agent sessions merged across every attached agent, or for one agent with agent_id. Each session carries agent_id.",
                    "responses": ok_response()
                },
                "post": {
                    "summary": "Create a new AI agent session",
                    "description": "Spawns an agent conversation session with the given model, mode, agent_id and workspace directory. Absent agent_id means the primary agent.",
                    "requestBody": json_body(object_schema(&[("directory", "string"), ("mode", "string"), ("agent_id", "string")], &[])),
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions/{asid}": {
                "get": {
                    "summary": "Get details of an AI agent session",
                    "parameters": [path_param("asid")],
                    "responses": ok_response()
                },
                "delete": {
                    "summary": "Delete an AI agent session",
                    "parameters": [path_param("asid")],
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions/{asid}/prompt": {
                "post": {
                    "summary": "Send a prompt to an AI agent session",
                    "description": "Submits a user prompt to the agent, optionally specifying model override and reasoning effort.",
                    "parameters": [path_param("asid")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["parts"],
                        "properties": {
                            "parts": { "type": "array", "items": { "type": "object" } },
                            "model": { "type": "string" },
                            "reasoning_effort": { "type": "string", "description": "Reasoning effort tier supported by the model (dynamic string from third-party provider)" }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions/{asid}/events": {
                "get": {
                    "summary": "Stream agent session events via Server-Sent Events",
                    "description": "Streams real-time agent execution events, thoughts, tool calls, and text deltas.",
                    "parameters": [path_param("asid")],
                    "responses": {
                        "200": { "description": "SSE stream of agent session events" }
                    }
                }
            },
            "/api/ws": {
                "get": {
                    "summary": "Agent events for many sessions over one WebSocket",
                    "description": "Upgrade to a WebSocket that carries the same agent events as the per-session SSE stream, for the sessions the client subscribes to (or all). Authenticated like the SSE stream; frames are sealed per connection on an encrypted device. See docs/agent-api.md, \"WebSocket events\". Announced as ws_events.",
                    "responses": {
                        "101": { "description": "Switched to the WebSocket protocol" },
                        "429": { "description": "too_many_connections: the gateway-wide socket cap is reached" }
                    }
                }
            },
            "/api/agent-sessions/{asid}/interrupt": {
                "post": {
                    "summary": "Interrupt active agent turn",
                    "description": "Cancels running model completion or tool execution for the session.",
                    "parameters": [path_param("asid")],
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions/{asid}/mode": {
                "post": {
                    "summary": "Switch the session's mode",
                    "description": "Switches the mode (persona) the session runs in, such as build or plan. The body is {\"mode\": \"build\"} or a bare \"build\".",
                    "parameters": [path_param("asid")],
                    "requestBody": json_body(object_schema(&[("mode", "string")], &["mode"])),
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions/{asid}/permissions/{reqId}/reply": {
                "post": {
                    "summary": "Reply to a pending agent tool execution permission request",
                    "parameters": [path_param("asid"), path_param("reqId")],
                    "requestBody": json_body(json!({
                        "type": "object",
                        "required": ["decision"],
                        "properties": {
                            "decision": { "type": "string", "enum": ["allow", "allow_always", "deny"] },
                            "message": { "type": "string" }
                        }
                    })),
                    "responses": ok_response()
                }
            },
            "/api/agent-sessions/{asid}/vcs/diff": {
                "get": {
                    "summary": "Get git diff produced by this agent session",
                    "parameters": [path_param("asid")],
                    "responses": ok_response()
                }
            }
        }
    })
}

fn task_steps_schema() -> Value {
    json!({
        "type": "array",
        "description": "What happened, in order: worktree, workspace, agent, prompt, and rollback if one was needed. Present on success and on a partial run alike, so a client never has to guess how far a request got.",
        "items": {
            "type": "object",
            "required": ["step", "status"],
            "properties": {
                "step": { "type": "string", "enum": ["worktree", "workspace", "agent", "prompt", "rollback"] },
                "status": { "type": "string", "enum": ["ok", "skipped", "failed", "rolled_back"] },
                "detail": { "type": "object" },
                "reason": { "type": "string" },
                "error": object_schema(&[("code", "string"), ("message", "string")], &["code", "message"])
            }
        }
    })
}

fn task_result_schema() -> Value {
    json!({
        "type": "object",
        "required": ["workspace_id", "pane_id", "agent", "agent_started", "prompt_submitted", "steps"],
        "properties": {
            "workspace_id": { "type": "string" },
            "pane_id": { "type": "string", "description": "Where the agent runs; also the target for the agent endpoints" },
            "worktree_path": { "type": ["string", "null"], "description": "Absent when no branch_name was given" },
            "branch": { "type": ["string", "null"] },
            "agent": { "type": "string" },
            "reused_worktree": { "type": "boolean", "description": "True when the branch already had a checkout, which is what makes a retry safe" },
            "agent_started": { "type": "boolean" },
            "agent_instance_id": { "type": ["string", "null"], "description": "Opaque identity of the ready agent conversation. Never correlate assignment history by pane id alone." },
            "prompt_submitted": { "type": "boolean" },
            "steps": task_steps_schema()
        }
    })
}

fn task_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Every step succeeded",
        "content": { "application/json": { "schema": task_result_schema() } }
    });
    responses["207"] = json!({
        "description": "Somewhere to work was created, but a later step failed -- the agent did not come up, or the prompt did not land. The body is the same shape as a 200; the failed step names what went wrong. Nothing is rolled back here: the checkout and pane are usable.",
        "content": { "application/json": { "schema": task_result_schema() } }
    });
    responses["400"] = json!({
        "description": "Unknown agent kind, malformed branch name, repo_path that is not a git checkout, or Herdr refusing the request. Nothing was created; a worktree this request made and could not attach a workspace to is removed again."
    });
    responses["403"] =
        json!({ "description": "repo_path is not inside a workspace this session has open" });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["502"] = json!({ "description": "Herdr is unavailable, or answered without the fields its schema promises" });
    responses
}

fn agents_catalog_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Agent kinds, sorted, with PATH availability",
        "content": { "application/json": { "schema": json!({
            "type": "object",
            "required": ["agents", "default_startup_timeout_ms"],
            "properties": {
                "agents": { "type": "array", "items": json!({
                    "type": "object",
                    "required": ["kind", "command", "available", "source"],
                    "properties": {
                        "kind": { "type": "string" },
                        "command": { "type": "string" },
                        "available": { "type": "boolean" },
                        "path": { "type": ["string", "null"] },
                        "source": { "type": "string", "enum": ["builtin", "config"] }
                    }
                }) },
                "default_startup_timeout_ms": { "type": "integer" }
            }
        }) } }
    });
    responses
}

fn capabilities_discovery_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Terminal and Agents dual-plane capabilities discovery",
        "content": { "application/json": { "schema": json!({
            "type": "object",
            "required": ["serverVersion", "protocolVersion", "planes", "capabilities"],
            "properties": {
                "serverVersion": { "type": "string", "description": "Gateway binary semver" },
                "protocolVersion": { "type": "string", "description": "Protocol revision date (e.g. 2026-09-28)" },
                "planes": {
                    "type": "object",
                    "required": ["terminal", "agents"],
                    "properties": {
                        "terminal": {
                            "type": "object",
                            "required": ["supported", "activeBackend", "availableBackends", "features"],
                            "properties": {
                                "supported": { "type": "boolean", "description": "Whether any terminal multiplexer is available" },
                                "activeBackend": { "type": ["string", "null"], "description": "Currently active multiplexer backend (tmux, herdr, conpty)" },
                                "availableBackends": { "type": "array", "items": { "type": "string" } },
                                "degradedReason": { "type": ["string", "null"], "description": "Reason terminal plane is unavailable if supported is false" },
                                "features": {
                                    "type": "object",
                                    "properties": {
                                        "sessionList": { "type": "boolean" },
                                        "splitPanes": { "type": "boolean" },
                                        "resize": { "type": "boolean" },
                                        "mouseReporting": { "type": "boolean" },
                                        "broadcast": { "type": "boolean" }
                                    }
                                }
                            }
                        },
                        "agents": agents_plane_schema(),
                        "ssh": {
                            "type": "object",
                            "properties": {
                                "supported": { "type": "boolean", "description": "Whether SSH plane is available" },
                                "tunnelSupported": { "type": "boolean", "description": "Whether SSH gateway tunnels are supported" },
                                "pushTokenSupported": { "type": "boolean", "description": "Whether push token registration is supported" }
                            }
                        }
                    }
                },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            }
        }) } }
    });
    responses
}

fn simple_endpoint(summary: &str) -> Value {
    json!({
        "summary": summary,
        "responses": ok_response()
    })
}

/// What a client can rely on from a pane's geometry, and what it cannot.
///
/// Written once and attached to both pane routes, because a reader who finds
/// `width: null` on one of them needs the same explanation on the other.
const PANE_GEOMETRY_DOC: &str = "Geometry fields are optional and differ by backend, so a client has to cope with `null` on any of them. tmux reports `width`, `height`, `alternate_on`, `cursor_x` and `cursor_y` for every pane. herdr reports `height` (its `scroll.viewport_rows`, which is the pane's real row count) and nothing else: its socket API has no pane width, no alternate-screen flag and no cursor position at protocol 20, so those four arrive `null` and a client that needs columns must still measure them from the text it reads. `cursor_x` and `cursor_y` are zero-based, column then row, from the top left of the viewport rather than of the scrollback.";

/// The geometry half of a pane, for the two routes that answer with panes.
/// Not the whole pane -- only the fields whose nullability a client has to
/// dispatch on.
fn pane_geometry_response() -> Value {
    json!({
        "200": {
            "description": "OK",
            "content": { "application/json": { "schema": {
                "type": "object",
                "properties": { "result": { "type": "object", "properties": { "panes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "pane_id": { "type": "string" },
                            "width": { "type": ["integer", "null"], "description": "Columns. Null on herdr." },
                            "height": { "type": ["integer", "null"], "description": "Rows." },
                            "cursor_x": { "type": ["integer", "null"], "description": "Zero-based cursor column. Null where the backend has no cursor to report." },
                            "cursor_y": { "type": ["integer", "null"], "description": "Zero-based cursor row." },
                            "scroll": { "type": "object", "properties": {
                                "viewport_rows": { "type": ["integer", "null"] },
                                "max_offset_from_bottom": { "type": ["integer", "null"] },
                                "alternate_on": { "type": ["boolean", "null"], "description": "Whether the pane's program owns an alternate screen. Null on herdr." }
                            } }
                        },
                        "required": ["pane_id"]
                    }
                } } } }
            } } }
        }
    })
}

fn session_endpoint(summary: &str) -> Value {
    json!({
        "summary": summary,
        "parameters": [path_param("sessionId")],
        "responses": ok_response()
    })
}

fn resource_endpoint(summary: &str, resource_param: &str) -> Value {
    json!({
        "summary": summary,
        "parameters": [path_param("sessionId"), path_param(resource_param)],
        "responses": ok_response()
    })
}

fn object_schema(properties: &[(&str, &str)], required: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(name, ty)| ((*name).to_owned(), json!({ "type": ty })))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "required": required,
        "properties": properties
    })
}

fn path_param(name: &str) -> Value {
    json!({
        "name": name,
        "in": "path",
        "required": true,
        "schema": { "type": "string" }
    })
}

fn query_param(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "in": "query",
        "required": false,
        "description": description,
        "schema": { "type": "string" }
    })
}

fn multipart_file_body() -> Value {
    json!({
        "required": true,
        "content": {
            "multipart/form-data": {
                "schema": {
                    "type": "object",
                    "required": ["file"],
                    "properties": { "file": { "type": "string", "format": "binary" } }
                }
            }
        }
    })
}

fn upload_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Stored upload: `path` is the host path for the agent to read, `url` the gateway route the app reads the same bytes from",
        "content": { "application/json": { "schema": object_schema(
            &[("path", "string"), ("url", "string"), ("name", "string"), ("size", "integer"), ("mime", "string")],
            &["path", "url", "name", "size", "mime"],
        ) } }
    });
    responses["400"] =
        json!({ "description": "Malformed multipart body, or no usable file field" });
    responses["413"] = json!({ "description": "Upload is larger than 25 MiB" });
    responses["415"] = json!({ "description": "Content is an executable or script, or not an accepted image type" });
    responses
}

fn upload_content_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The upload's bytes, with the sniffed type in content-type and `cache-control: private`",
        "content": { "*/*": { "schema": { "type": "string", "format": "binary" } } }
    });
    responses["404"] = json!({ "description": "Unknown, unreadable, expired, or not a plain file directly inside the upload directory" });
    responses
}

fn asset_schema() -> Value {
    json!({
        "type": "object",
        "required": ["id", "path", "name", "kind", "mime", "size", "modified_unix_ms", "origin", "previewable"],
        "properties": {
            "id": { "type": "string" },
            "path": { "type": "string" },
            "name": { "type": "string" },
            "kind": { "type": "string", "enum": ["image", "markdown", "text", "pdf", "binary"] },
            "mime": { "type": "string" },
            "size": { "type": "integer" },
            "modified_unix_ms": { "type": "integer" },
            "origin": object_schema(
                &[("session_id", "string"), ("workspace_id", "string"), ("pane_id", "string"), ("root", "string")],
                &["session_id"],
            ),
            "previewable": { "type": "boolean" }
        }
    })
}

fn content_envelope_schema(data: Value) -> Value {
    json!({
        "type": "object",
        "required": ["schema_version", "capabilities", "data"],
        "properties": {
            "schema_version": { "type": "string", "const": CONTENT_SCHEMA_VERSION },
            "capabilities": object_schema(
                &[("parts", "boolean"), ("assets", "boolean"), ("image_upload", "boolean"), ("composer", "boolean")],
                &["parts", "assets", "image_upload", "composer"],
            ),
            "data": data
        }
    })
}

fn assets_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Recent assets, newest first",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "assets"],
            "properties": {
                "session_id": { "type": "string" },
                "assets": { "type": "array", "items": asset_schema() },
                "limit": { "type": "integer" },
                "since": { "type": ["integer", "null"], "description": "Unix milliseconds, echoed back" },
                "path": { "type": ["string", "null"], "description": "The requested exact path, echoed back on a path lookup" },
                "roots": { "type": "array", "items": { "type": "string" } }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses
}

/// One part on the wire. Deliberately loose about the payload and strict about
/// the two fields the contract rests on: `type`, so a client can dispatch, and
/// `fallback_text`, so it can render whatever it could not dispatch on.
fn part_schema() -> Value {
    json!({
        "type": "object",
        "required": ["type", "fallback_text"],
        "properties": {
            "type": {
                "type": "string",
                "enum": ["text", "tool-block", "diff", "todo", "table", "status", "prompt", "asset-ref", "approval"],
                "description": "Closed set. A client that does not know a value renders fallback_text; new types arrive on a minor bump"
            },
            "fallback_text": { "type": "string", "description": "The source lines verbatim" },
            "range": object_schema(&[("start", "integer"), ("end", "integer")], &["start", "end"]),
            "markdown": { "type": "string", "description": "text: prose, rendered best-effort" },
            "tool": { "type": "string", "description": "tool-block: the tool the agent named" },
            "input": { "type": "string", "description": "tool-block: what it was called with" },
            "result": { "type": "array", "items": { "type": "string" }, "description": "tool-block: the result lines" },
            "status": { "type": "string", "enum": ["ok", "error", "running"], "description": "tool-block: read off the first result line; running means no result yet" },
            "truncated": { "type": "boolean", "description": "tool-block: the agent printed an ellipsis, so this is not all of it" },
            "file": { "type": ["string", "null"], "description": "diff: the file the block edited, when the tool named one" },
            "hunks": { "type": "array", "items": { "type": "string" }, "description": "diff: the numbered source lines" },
            "items": {
                "type": "array",
                "description": "todo: the checklist",
                "items": object_schema(&[("text", "string"), ("done", "boolean")], &["text", "done"])
            },
            "text": { "type": "string", "description": "status and prompt: the line's content" },
            "spinner": { "type": "boolean", "description": "status: the line is one of the agent's animated frames" },
            "approval_id": { "type": "string", "description": "approval: what POST .../approval answers. Only a source that reports approval state can raise one; a pane read through a marker dictionary carries its menu on the approvals endpoint instead" },
            "prompt": { "type": "string", "description": "approval: the question, written by the gateway from the protocol's own action name and never quoted out of a terminal" },
            "context": { "type": "array", "items": { "type": "string" }, "description": "approval: what is being asked for -- the command, the path, the host -- verbatim from the protocol" },
            "options": {
                "type": "array",
                "description": "approval: the answers, labelled by the gateway so an agent's own wording (which routinely embeds the command) never travels",
                "items": object_schema(&[("index", "integer"), ("label", "string"), ("decision", "string")], &["index", "label", "decision"])
            }
        }
    })
}

/// What a pane's composer can offer. Absent from `data.pane` entirely when the
/// gateway has no command table for the agent, which is how a client tells "no
/// table" from "no commands".
fn composer_schema() -> Value {
    json!({
        "type": "object",
        "description": "Absent for an agent the gateway has no table for",
        "required": ["version", "table", "slash_commands", "file_mentions"],
        "properties": {
            "version": { "type": "integer", "description": "Bumped whenever a builtin table changes, so a client can cache this" },
            "table": { "type": "string", "description": "Which table answered, the same id the part dictionaries use" },
            "captured_from": { "type": "string", "description": "The agent release the builtin table was read off" },
            "file_mentions": { "type": "boolean", "description": "Whether @ in the composer means 'mention a file' to this agent" },
            "slash_commands": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "description", "source"],
                    "properties": {
                        "name": { "type": "string", "description": "The literal text to send, leading slash included" },
                        "description": { "type": "string" },
                        "args_hint": { "type": ["string", "null"], "description": "What may follow the command; null means it runs exactly as typed, so a client may send it on one tap" },
                        "source": { "type": "string", "enum": ["builtin", "workspace"], "description": "builtin: the gateway's table for this agent. workspace: a skill or command file found in the pane's workspace, which wins over a builtin of the same name" }
                    }
                }
            }
        }
    })
}

fn file_search_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Fuzzy path matches inside the pane's workspace, best first",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "files", "root"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "query": { "type": "string", "description": "The query, echoed back" },
                "limit": { "type": "integer", "description": "The clamped limit, echoed back" },
                "root": { "type": ["string", "null"], "description": "The workspace directory every path is relative to, or null when this pane has none the gateway will look in" },
                "files": {
                    "type": "array",
                    "items": object_schema(&[("path", "string"), ("name", "string"), ("kind", "string")], &["path", "name", "kind"])
                }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses
}

fn repo_summary_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "description": "The checkout's branch line, or null when the pane is not inside one",
        "required": ["toplevel", "detached", "changed_files"],
        "properties": {
            "toplevel": { "type": "string" },
            "branch": { "type": ["string", "null"] },
            "upstream": { "type": ["string", "null"] },
            "ahead": { "type": ["integer", "null"] },
            "behind": { "type": ["integer", "null"] },
            "detached": { "type": "boolean" },
            "head": { "type": ["string", "null"], "description": "Abbreviated commit id; null on an unborn branch" },
            "changed_files": { "type": "integer", "description": "Every changed file, before any list cap" }
        }
    })
}

fn pane_context_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "Where the pane is and what runs in it",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "cwd", "cwd_in_fence", "git", "agent"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "cwd": { "type": ["string", "null"] },
                "cwd_in_fence": { "type": "boolean", "description": "Whether the file and asset routes will look inside cwd" },
                "git": repo_summary_schema(),
                "agent": {
                    "type": ["object", "null"],
                    "required": ["kind", "status", "profile"],
                    "properties": {
                        "kind": { "type": "string" },
                        "status": { "type": "string", "enum": ["starting", "working", "idle", "blocked", "done", "unknown"] },
                        "foreground_command": { "type": ["string", "null"] },
                        "profile": { "type": "boolean", "description": "Whether this gateway has a key row and interrupt key for the agent" }
                    }
                }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses
}

fn git_status_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The checkout's changed files, working tree against HEAD",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "repo", "truncated", "files"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "repo": repo_summary_schema(),
                "truncated": { "type": "boolean", "description": "The list stopped at the cap" },
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["path", "status", "staged", "unstaged", "binary"],
                        "properties": {
                            "path": { "type": "string", "description": "Relative to toplevel" },
                            "old_path": { "type": ["string", "null"], "description": "For a rename or copy" },
                            "status": { "type": "string", "enum": ["added", "modified", "deleted", "renamed", "copied", "untracked", "conflicted", "type_changed"] },
                            "staged": { "type": "boolean" },
                            "unstaged": { "type": "boolean" },
                            "binary": { "type": "boolean" },
                            "added": { "type": ["integer", "null"] },
                            "removed": { "type": ["integer", "null"] }
                        }
                    }
                }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["502"] = json!({ "description": "git failed" });
    responses["504"] = json!({ "description": "git took more than five seconds" });
    responses
}

fn git_diff_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "One page of one file's unified patch",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "path", "binary", "from", "end", "total_lines", "truncated", "patch"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "path": { "type": "string" },
                "binary": { "type": "boolean" },
                "from": { "type": "integer" },
                "end": { "type": "integer", "description": "One past the last line in this page; pass as from for the next" },
                "total_lines": { "type": "integer" },
                "truncated": { "type": "boolean", "description": "Another page follows" },
                "patch": { "type": "string", "description": "Unified diff text, a/ b/ prefixes, no colour" }
            }
        })) } }
    });
    responses["400"] = json!({ "description": "path is not a relative path inside the checkout" });
    responses["404"] = json!({ "description": "Unknown session, no checkout (no_repository), or nothing at that path (no_such_path)" });
    responses["502"] = json!({ "description": "git failed" });
    responses["504"] = json!({ "description": "git took more than five seconds" });
    responses
}

fn parts_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The pane's transcript as ordered parts",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "parts", "pane"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "source": { "type": "string", "enum": ["recent-unwrapped", "native"], "description": "recent-unwrapped: the pane's own text, which is what the dictionaries key off. native: the agent's own protocol answered, and range spans the adapter's rendering -- which is the parts' fallback_text joined by newlines -- rather than terminal rows" },
                "lines": { "type": "integer", "description": "How many lines were read, echoed back" },
                "revision": { "type": ["integer", "null"], "description": "Herdr's pane revision for this read, when it reported one" },
                "pane": {
                    "type": "object",
                    "required": ["pane_id", "parts", "image_input"],
                    "properties": {
                        "pane_id": { "type": "string" },
                        "agent": { "type": ["string", "null"], "description": "What Herdr reports is running in the pane" },
                        "parts": { "type": "string", "enum": ["native", "dictionary", "text"], "description": "native: the agent's own protocol answered. dictionary: typed parts read off the screen. text: no dictionary covers this pane, everything degraded to prose" },
                        "dictionary": { "type": ["string", "null"], "description": "Which dictionary normalized it, for cache keys and bug reports" },
                        "native": {
                            "type": ["object", "null"],
                            "description": "Null unless a protocol actually answered, so a client cannot mistake 'an adapter could have read this pane' for 'an adapter did'",
                            "properties": {
                                "protocol": { "type": ["string", "null"], "description": "What the agent's own release calls it, for bug reports" },
                                "version": { "type": ["string", "null"], "description": "The version the agent's server reported" },
                                "session": { "type": ["string", "null"], "description": "The agent's own session identity, so a client can tell one native read from the next" }
                            }
                        },
                        "image_input": { "type": "string", "description": "How an image reaches this agent; file-path means upload first, then send the path" },
                        "composer": composer_schema()
                    }
                },
                "parts": { "type": "array", "items": part_schema() }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["502"] =
        json!({ "description": "Herdr is unavailable, or the pane could not be read" });
    responses
}

fn approval_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The pane's approval state, in the content-model envelope",
        "content": { "application/json": { "schema": content_envelope_schema(json!({
            "type": "object",
            "required": ["session_id", "pane_id", "state", "approval", "pane"],
            "properties": {
                "session_id": { "type": "string" },
                "pane_id": { "type": "string" },
                "state": { "type": "string", "enum": ["pending", "idle"] },
                "pane": {
                    "type": "object",
                    "required": ["pane_id", "approvals"],
                    "properties": {
                        "pane_id": { "type": "string" },
                        "agent": { "type": ["string", "null"] },
                        "approvals": { "type": "string", "enum": ["menu"], "description": "How the approval was obtained: menu means it was read off what the agent drew" }
                    }
                },
                "approval": {
                    "type": ["object", "null"],
                    "required": ["fingerprint", "prompt", "options"],
                    "properties": {
                        "fingerprint": { "type": "string", "description": "Stable identity of this question with these answers" },
                        "prompt": { "type": "string", "description": "The question, verbatim" },
                        "tool": { "type": ["string", "null"], "description": "The tool the request is about, when the agent named one" },
                        "context": { "type": "array", "items": { "type": "string" }, "description": "The lines the agent drew around the question, verbatim and capped" },
                        "hint": { "type": ["string", "null"], "description": "The agent's own key-hint footer" },
                        "range": object_schema(&[("start", "integer"), ("end", "integer")], &["start", "end"]),
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["index", "label", "selected", "decision"],
                                "properties": {
                                    "index": { "type": "integer", "description": "The number the agent printed, which is what POST takes" },
                                    "label": { "type": "string", "description": "The answer verbatim" },
                                    "selected": { "type": "boolean", "description": "Where the agent's cursor is" },
                                    "decision": { "type": "string", "enum": ["allow", "allow_always", "deny", "other"] }
                                }
                            }
                        }
                    }
                },
                "resolved": { "type": "boolean", "description": "POST only: whether the menu was gone after answering" },
                "sent_keys": { "type": "array", "items": { "type": "string" }, "description": "POST only: the keys the gateway sent, including a deferred Enter when one was needed" },
                "answered": {
                    "type": "object",
                    "description": "POST only: which option was taken",
                    "properties": {
                        "fingerprint": { "type": "string" },
                        "index": { "type": "integer" },
                        "decision": { "type": "string" }
                    }
                }
            }
        })) } }
    });
    responses["404"] = json!({ "description": "Unknown session" });
    responses["409"] = json!({ "description": "The pane is not waiting on an approval, or it is waiting on a different one" });
    responses["502"] =
        json!({ "description": "Herdr is unavailable, or the pane could not be read" });
    responses
}

fn asset_content_responses() -> Value {
    let mut responses = ok_response();
    responses["200"] = json!({
        "description": "The asset's bytes, with the sniffed type in content-type and x-asset-kind",
        "content": { "*/*": { "schema": { "type": "string", "format": "binary" } } }
    });
    responses["404"] = json!({ "description": "Unknown asset, or a path that does not resolve inside a session workspace root" });
    responses["413"] = json!({ "description": "The asset is larger than 10 MiB" });
    responses["415"] = json!({
        "description": "Binary asset: metadata only, no preview",
        "content": { "application/json": { "schema": {
            "type": "object",
            "properties": { "error": { "type": "object" }, "asset": asset_schema() }
        } } }
    });
    responses
}

fn json_body(schema: Value) -> Value {
    json!({
        "required": true,
        "content": { "application/json": { "schema": schema } }
    })
}

fn ok_response() -> Value {
    json!({
        "200": {
            "description": "Successful response",
            "content": { "application/json": { "schema": {} } }
        },
        "401": { "description": "Missing or invalid authorization" },
        "403": { "description": "Invalid token" },
        "502": { "description": "Terminal backend unavailable or returned an error" }
    })
}

pub const DOCS_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Terminal Gateway API Docs</title>
  </head>
  <body>
    <script id="api-reference" data-url="/openapi.json"></script>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
  </body>
</html>
"#;
