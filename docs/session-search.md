# Session search

`GET /api/search` searches the content of every agent session on this
computer: what the user wrote, what the agent answered, and the headline of
each tool call (its command, file path or search pattern -- never its output
or a file's contents). Capability: `session_search_v1`.

## Request

```
GET /api/search?q=<text>&limit=20&cursor=<next_cursor>&agent_id=<id>&directory=<abs path>
Authorization: Bearer <device token>
```

| parameter   | rule                                                                 |
|-------------|----------------------------------------------------------------------|
| `q`         | required; 1-200 characters after trimming; no control characters      |
| `limit`     | 1-50, default 20                                                      |
| `cursor`    | the `next_cursor` of the previous page                                |
| `agent_id`  | only this agent's sessions (`codex`, `claude`, ...)                   |
| `directory` | absolute; sessions in this directory or below it                      |

`q` is split on whitespace; every term must match, each as a
case-insensitive substring. There is no word segmentation, so `数据库`,
`テスト` and `ser mod` all match inside longer text. Terms of three or more
characters use the trigram index; shorter ones (a two-character Chinese word)
are matched by scanning the rows the other terms selected, or all rows newest
first when every term is short.

Errors: `400 invalid_query | invalid_limit | invalid_cursor |
invalid_agent_id | invalid_directory`, `401`, `403` (another gateway's token
is `peer_forbidden`), `503 search_unavailable` (the index did not open).

## Response

```json
{"data": {
  "items": [{
    "asid": "…", "agent_id": "codex", "title": "…", "directory": "/…",
    "parent_asid": null, "item_id": "…", "message_id": "…",
    "role": "user", "kind": "text", "time_ms": 1793000000000,
    "snippet": "…we moved the billing tables…", "highlights": [[13, 20]],
    "session_hits": 5
  }],
  "next_cursor": "20",
  "indexing": {"done": 312, "total": 340}
}}
```

- Hits are ranked by bm25 relevance weighted by recency (half weight after
  30 days, never below 30%) and slightly towards user prompts. At most three
  hits come from one session; a session's hits are listed together, in the
  order of the session's best hit. `session_hits` is how many of the
  session's rows matched among the 1,000 best rows ranked.
- `highlights` are `[start, end)` offsets into `snippet` in UTF-16 code units,
  which is how JavaScript and Swift index strings.
- `item_id` names the timeline row as the session's own timeline does
  (`GET /api/agent-sessions/{asid}/…`), so a client can scroll to it after
  opening the session. Claude Code rows carry the session's update time, not
  the row's own.
- `indexing` counts the sessions whose index is current out of the sessions
  known. While `done < total` the first build (or a catch-up) is running and
  results are partial.

To open a hit, open its session as usual (`asid`, `agent_id`) and find the row
by `item_id`; fall back to `message_id` when the row is outside the loaded
window.

## How the index is built

The index is a SQLite FTS5 table (trigram tokenizer) in
`<state>/store/search.sqlite3`, encrypted with the store key when the store is
encrypted. It is derived data: a file that does not open is deleted and built
again. It never leaves the machine.

Each agent's history is read where it already lies, never by opening the
session in its agent (no ACP child is started, no Codex thread resumed, no
pagination cursor of the App moved):

| agent    | sessions                      | history                                       | incremental by           |
|----------|-------------------------------|-----------------------------------------------|--------------------------|
| claude   | adapter list (transcripts)    | the transcript through the shared ACP projection | length + mtime         |
| codex    | adapter list (`thread/list`)  | the rollout file's completed items            | byte offset (append-only) |
| opencode | its SQLite store, read-only   | `session_message` rows through the adapter mapper | `time_updated` + count |
| pi       | adapter list (session files)  | the adapter's file read                       | session update time      |

Sessions of the other agents (DeepSeek, T3, FX, Qoder, Cursor, Antigravity)
are indexed from what reaches this gateway live: their domain events and the
gateway's own mirror of the session, i.e. what the App was shown. Their
history before that is not read, because reading it would start an agent
process or move the App's history cursors.

The first build starts 20 s after the gateway, newest sessions first, one
session at a time, pausing after each read as long as the read took (about
half of one core at most); the writer runs on a lower-priority thread. A
catch-up pass every 5 minutes compares each session's stamp with the
watermark stored beside its rows, reads what changed and drops sessions their
agent no longer lists. Between passes, sessions the domain events touched are
read again once quiet for 10 s (at most 2 minutes after the first event).
Watermarks are stored per session, so a restart resumes where it stopped; a
large Codex rollout is read 32 MiB at a time.

Rows are served through the transcript's hiding rules (`tasks::transcript`):
a task session's completion instruction and consumed markers, and
skill-loading rows, are never indexed. Reasoning, system notices, synthetic
messages and tool output are not indexed.

## Linked computers

Only this computer is searched. A federated search would call
`GET /api/search` on each linked computer through its peer client and merge
the pages by score; `SearchService::search` is that seam.
