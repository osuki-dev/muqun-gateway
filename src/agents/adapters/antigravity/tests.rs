use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::driver::NOT_SIGNED_IN;
use super::{store, AntigravityDriver};
use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::domain::{AgentSessionStatus, ModelRef};
use crate::agents::ports::agent::{AgentError, AgentPort};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn scratch() -> Scratch {
    let path = std::env::temp_dir().join(format!("muqun-antigravity-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(path.join("conversations")).unwrap();
    Scratch(path)
}

#[test]
fn listing_reads_top_level_conversations_with_folders_newest_first() {
    let root = scratch();
    let db = rusqlite::Connection::open(root.0.join("conversation_summaries.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE conversation_summaries (conversation_id text PRIMARY KEY, title text NOT NULL DEFAULT '',
           preview text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0,
           last_modified_time datetime NOT NULL, workspace_uris text NOT NULL,
           parent_conversation_id text NOT NULL DEFAULT '', nesting_depth integer NOT NULL DEFAULT 0,
           last_user_input_time datetime NOT NULL);",
    )
    .unwrap();
    let never = "0001-01-01 00:00:00+00:00";
    // (id, title, preview, steps, modified, user input, folders, parent, depth)
    for row in [
        (
            "a-titled",
            "Fix the build",
            "",
            4,
            "2026-10-30 08:00:00.5+00:00",
            never,
            r#"["file:///work/app"]"#,
            "",
            0,
        ),
        (
            "b-untitled",
            "",
            "Reply with PONG",
            2,
            never,
            "2026-10-31 09:00:00+00:00",
            r#"["file:///work/my%20app"]"#,
            "",
            0,
        ),
        (
            "c-subagent",
            "Child",
            "",
            3,
            "2026-10-31 10:00:00+00:00",
            never,
            r#"["file:///work/app"]"#,
            "a-titled",
            1,
        ),
        (
            "d-empty",
            "",
            "",
            0,
            "2026-10-31 10:00:00+00:00",
            never,
            r#"["file:///work/app"]"#,
            "",
            0,
        ),
        (
            "e-nofolder",
            "No folder",
            "",
            2,
            "2026-10-31 10:00:00+00:00",
            never,
            "",
            "",
            0,
        ),
        (
            "f-undated",
            "Undated",
            "",
            1,
            never,
            never,
            r#"["file:///work/app"]"#,
            "",
            0,
        ),
    ] {
        db.execute(
            "INSERT INTO conversation_summaries VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![row.0, row.1, row.2, row.3, row.4, row.6, row.7, row.8, row.5],
        )
        .unwrap();
    }
    drop(db);
    let stop = CancellationToken::new();
    assert_eq!(
        store::list_at(&root.0, None, &stop).unwrap(),
        [
            json!({"sessionId":"b-untitled","cwd":"/work/my app","title":"Reply with PONG","updatedAt":"2026-10-31T09:00:00.000Z"}),
            json!({"sessionId":"a-titled","cwd":"/work/app","title":"Fix the build","updatedAt":"2026-10-30T08:00:00.500Z"}),
            json!({"sessionId":"f-undated","cwd":"/work/app","title":"Undated"}),
        ]
    );
    let scoped = store::list_at(&root.0, Some("/work/app"), &stop).unwrap();
    assert_eq!(scoped.len(), 2);
    let absent = Path::new("/nonexistent-muqun-antigravity");
    assert!(store::list_at(absent, None, &stop).unwrap().is_empty());
}

/// A length-delimited protobuf field; keys and lengths here fit two bytes.
fn field(number: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for mut value in [number << 3 | 2, bytes.len() as u64] {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
    out.extend_from_slice(bytes);
    out
}

#[test]
fn recovery_evidence_is_one_entry_per_user_turn_with_the_last_reply() {
    let root = scratch();
    let db = rusqlite::Connection::open(root.0.join("conversations").join("conv-1.db")).unwrap();
    db.execute_batch("CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer NOT NULL, step_payload blob);")
        .unwrap();
    // A varint field ahead of the text checks that other fields are skipped.
    let user = |text: &str| [vec![0x08, 0x0e], field(19, &field(2, text.as_bytes()))].concat();
    let reply = |text: &str| field(20, &field(1, text.as_bytes()));
    for (idx, kind, payload) in [
        (0, 7, b"setup".to_vec()),
        (1, 14, user("first")),
        (2, 15, reply("one")),
        (3, 14, user("second")),
        (4, 21, b"tool".to_vec()),
        (5, 15, reply("")),
        (6, 15, reply("two")),
    ] {
        db.execute(
            "INSERT INTO steps VALUES (?1,?2,?3)",
            rusqlite::params![idx, kind, payload],
        )
        .unwrap();
    }
    drop(db);
    let stop = CancellationToken::new();
    let snapshot = store::snapshot_at(&root.0, "conv-1", &stop).unwrap();
    assert_eq!(snapshot.session_id, "conv-1");
    assert_eq!(
        snapshot.entries,
        [
            json!({"user":"first","replies":["one"],"steps":[14,15]}),
            json!({"user":"second","replies":["two"],"steps":[14,21,15,15]}),
        ]
    );
    assert_eq!(snapshot.last_user.as_deref(), Some("second"));
    assert_eq!(snapshot.last_assistant.as_deref(), Some("two"));
    assert!(store::snapshot_at(&root.0, "missing", &stop).is_err());
}

const FAKE_AGY: &str = r#"#!/usr/bin/env python3
import json, os, sys
def send(v): print(json.dumps(v), flush=True)
def log(v):
    with open('requests.jsonl', 'a') as f: f.write(json.dumps(v) + '\n')
config = {'models': {'availableModels': [{'modelId': 'flash-high', 'name': 'Flash (High)'}], 'currentModelId': 'flash-high'},
          'configOptions': [{'id': 'model', 'category': 'model', 'type': 'select', 'currentValue': 'flash', 'options': [{'value': 'flash', 'name': 'Flash'}]},
                            {'id': 'thought_level', 'category': 'thought_level', 'type': 'select', 'currentValue': 'high', 'options': [{'value': 'low', 'name': 'Low'}, {'value': 'high', 'name': 'High'}]}]}
authenticated = False
for line in sys.stdin:
    m = json.loads(line); method = m.get('method'); rid = m.get('id'); p = m.get('params', {})
    log(m)
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': rid, 'result': {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True, 'sessionCapabilities': {'list': {}, 'load': {}}}, 'authMethods': [{'id': 'oauth-personal', 'name': 'Log in with Google'}]}})
    elif method == 'authenticate':
        if os.path.exists('signed-out'):
            send({'jsonrpc': '2.0', 'id': rid, 'error': {'code': -32800, 'message': 'Sign-in was cancelled.'}})
        else:
            authenticated = True
            send({'jsonrpc': '2.0', 'id': rid, 'result': {}})
    elif not authenticated:
        send({'jsonrpc': '2.0', 'id': rid, 'error': {'code': -32000, 'message': 'Authentication required'}})
    elif method == 'session/new':
        send({'jsonrpc': '2.0', 'id': rid, 'result': dict(config, sessionId='conv-live')})
    elif method == 'session/set_config_option':
        for option in config['configOptions']:
            if option['id'] == p['configId']: option['currentValue'] = p['value']
        send({'jsonrpc': '2.0', 'id': rid, 'result': {'configOptions': config['configOptions']}})
    elif method == 'session/prompt':
        send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {'sessionId': p['sessionId'], 'update': {'sessionUpdate': 'agent_message_chunk', 'content': {'type': 'text', 'text': 'PONG'}}}})
        send({'jsonrpc': '2.0', 'id': rid, 'result': {'stopReason': 'end_turn', 'usage': {'inputTokens': 3, 'outputTokens': 1, 'totalTokens': 4}}})
"#;

#[tokio::test]
async fn login_gates_sessions_and_choices_reach_agy_with_muqun_tools() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch();
    let binary = root.0.join("agy");
    std::fs::write(&binary, FAKE_AGY).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let driver = AntigravityDriver::new(
        binary.to_string_lossy().into_owned(),
        root.0.to_string_lossy().into_owned(),
        Arc::new(MemoryMirror::for_agent("antigravity")),
        tokio::sync::broadcast::channel(256).0,
    );
    let muqun = json!([{"type":"http","name":"muqun","url":"http://127.0.0.1:1/mcp","headers":[]}]);
    driver.set_mcp_servers(muqun.clone());

    std::fs::write(root.0.join("signed-out"), b"").unwrap();
    match driver.create_session(None, None, None).await {
        Err(AgentError::NotAvailable(message)) => assert_eq!(message, NOT_SIGNED_IN),
        other => panic!("expected the sign-in refusal, got {other:?}"),
    }
    std::fs::remove_file(root.0.join("signed-out")).unwrap();

    let low = ModelRef {
        provider_id: "antigravity".into(),
        model_id: "flash".into(),
        variant: Some("low".into()),
    };
    let info = driver.create_session(None, Some(&low), None).await.unwrap();
    let id = info.asid.0.clone();
    let catalog = driver
        .get_catalog_for_session(Some(&id), None)
        .await
        .unwrap();
    let models: Vec<_> = catalog
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(models, ["flash"], "1.3's suffixed ids are not offered");
    driver
        .send_prompt(&id, "Reply with exactly PONG", &[], None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while driver.get_session(&id).await.unwrap().status != AgentSessionStatus::Idle {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let requests: Vec<Value> = std::fs::read_to_string(root.0.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let of = |method: &str| -> Vec<&Value> {
        requests.iter().filter(|r| r["method"] == method).collect()
    };
    assert_eq!(
        of("session/new").len(),
        1,
        "no session opens while signed out"
    );
    assert_eq!(of("session/new")[0]["params"]["mcpServers"], muqun);
    assert!(of("session/set_config_option")
        .iter()
        .any(|r| r["params"]["configId"] == "thought_level" && r["params"]["value"] == "low"));
    assert_eq!(of("session/prompt").len(), 1);
    driver.shutdown().await;
}
