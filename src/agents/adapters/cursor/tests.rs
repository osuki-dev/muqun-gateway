use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::driver::{parse_login_status, NOT_SIGNED_IN};
use super::{store, CursorDriver};
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
    let path = std::env::temp_dir().join(format!("muqun-cursor-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    Scratch(path)
}

#[test]
fn listing_reads_titled_sessions_newest_first() {
    let root = scratch();
    // (folder, meta.json, has store)
    for (id, meta, store) in [
        (
            "a-old",
            r#"{"schemaVersion":1,"cwd":"/work/app","title":"Fix the build"}"#,
            true,
        ),
        (
            "b-new",
            r#"{"schemaVersion":1,"cwd":"/work/other","title":"PONG"}"#,
            true,
        ),
        (
            "c-untitled",
            r#"{"schemaVersion":1,"cwd":"/work/app"}"#,
            false,
        ),
        (
            "d-relative",
            r#"{"schemaVersion":1,"cwd":"work","title":"Relative"}"#,
            false,
        ),
        ("e-broken", "{", false),
        (
            "bad.id",
            r#"{"schemaVersion":1,"cwd":"/work/app","title":"Bad id"}"#,
            false,
        ),
    ] {
        let folder = root.0.join(id);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("meta.json"), meta).unwrap();
        if store {
            std::fs::write(folder.join("store.db"), b"").unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let stop = CancellationToken::new();
    let rows = store::list_at(&root.0, None, &stop).unwrap();
    let ids: Vec<_> = rows
        .iter()
        .map(|row| row["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["b-new", "a-old"]);
    assert_eq!(rows[1]["cwd"], "/work/app");
    assert_eq!(rows[1]["title"], "Fix the build");
    assert!(rows[1]["updatedAt"].as_str().unwrap().ends_with('Z'));
    let scoped = store::list_at(&root.0, Some("/work/app"), &stop).unwrap();
    assert_eq!(scoped.len(), 1);
    assert!(store::list_at(&root.0.join("absent"), None, &stop)
        .unwrap()
        .is_empty());
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
fn recovery_evidence_is_one_entry_per_prompt_with_the_last_reply() {
    let root = scratch();
    std::fs::create_dir_all(root.0.join("conv-1")).unwrap();
    let db = rusqlite::Connection::open(root.0.join("conv-1").join("store.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
    )
    .unwrap();
    let query = |text: &str| json!({"role":"user","content":[{"type":"text","text":format!("<timestamp>now</timestamp>\n<user_query>\n{text}\n</user_query>")}]});
    let reply = |text: &str| json!({"role":"assistant","content":[{"type":"reasoning","text":""},{"type":"text","text":text}]});
    let messages = [
        json!({"role":"system","content":"You are an AI coding assistant"}),
        json!({"role":"user","content":"<user_info>\nOS Version: test\n</user_info>"}),
        query("first"),
        reply("one"),
        query("second"),
        json!({"role":"assistant","content":[{"type":"tool-call","toolName":"Shell"}]}),
        json!({"role":"tool","content":[{"type":"tool-result"}]}),
        reply("two"),
    ];
    let mut root_blob = vec![0x50, 0x01];
    for (index, message) in messages.iter().enumerate() {
        let id = [index as u8; 32];
        db.execute(
            "INSERT INTO blobs VALUES (?1, ?2)",
            rusqlite::params![hex::encode(id), serde_json::to_vec(message).unwrap()],
        )
        .unwrap();
        root_blob.extend(field(1, &id));
    }
    root_blob.extend(field(9, b"file:///work/app"));
    db.execute(
        "INSERT INTO blobs VALUES ('root', ?1)",
        rusqlite::params![root_blob],
    )
    .unwrap();
    db.execute(
        "INSERT INTO meta VALUES ('0', ?1)",
        [hex::encode(
            br#"{"agentId":"conv-1","latestRootBlobId":"root"}"#,
        )],
    )
    .unwrap();
    drop(db);
    let stop = CancellationToken::new();
    let snapshot = store::snapshot_at(&root.0, "conv-1", &stop).unwrap();
    assert_eq!(
        snapshot.entries,
        [
            json!({"user":"first","replies":["one"],"roles":["user","assistant"]}),
            json!({"user":"second","replies":["two"],"roles":["user","assistant","tool","assistant"]}),
        ]
    );
    assert_eq!(snapshot.last_user.as_deref(), Some("second"));
    assert_eq!(snapshot.last_assistant.as_deref(), Some("two"));
    assert!(store::snapshot_at(&root.0, "missing", &stop).is_err());
}

#[test]
fn login_status_reads_only_an_explicit_answer() {
    for (output, expected) in [
        (
            &b"{\n  \"status\": \"authenticated\",\n  \"isAuthenticated\": true\n}\n"[..],
            Some(true),
        ),
        (b"note\n{\"isAuthenticated\": false}", Some(false)),
        (b"Not logged in\n", None),
        (b"{\"status\":\"unknown\"}", None),
    ] {
        assert_eq!(parse_login_status(output), expected);
    }
}

const FAKE_AGENT: &str = r#"#!/usr/bin/env python3
import json, os, sys
here = os.path.dirname(os.path.abspath(__file__))
def send(v): print(json.dumps(v), flush=True)
def log(v):
    with open(os.path.join(here, 'requests.jsonl'), 'a') as f: f.write(json.dumps(v) + '\n')
if sys.argv[1:] == ['status', '--format', 'json']:
    print(json.dumps({'status': 'x', 'isAuthenticated': not os.path.exists(os.path.join(here, 'signed-out'))}, indent=2))
    sys.exit(0)
assert sys.argv[1:] == ['acp'], sys.argv
models = [{'value': 'default[]', 'name': 'Auto'}, {'value': 'gpt-5.4-nano[reasoning=medium]', 'name': 'gpt-5.4-nano'}]
config = {'modes': {'currentModeId': 'agent', 'availableModes': [{'id': 'agent', 'name': 'Agent'}]},
          'models': {'currentModelId': 'default[]', 'availableModels': [{'modelId': m['value'], 'name': m['name']} for m in models]},
          'configOptions': [{'id': 'mode', 'category': 'mode', 'type': 'select', 'currentValue': 'agent', 'options': [{'value': 'agent', 'name': 'Agent'}]},
                            {'id': 'model', 'category': 'model', 'type': 'select', 'currentValue': 'default[]', 'options': models}]}
authenticated = False
for line in sys.stdin:
    m = json.loads(line); method = m.get('method'); rid = m.get('id'); p = m.get('params', {})
    log(m)
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': rid, 'result': {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True, 'promptCapabilities': {'image': True}, 'sessionCapabilities': {'list': {}}}, 'authMethods': [{'id': 'cursor_login', 'name': 'Cursor Login'}]}})
    elif method == 'authenticate':
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
        send({'jsonrpc': '2.0', 'id': rid, 'result': {'stopReason': 'end_turn'}})
"#;

#[tokio::test]
async fn signed_out_starts_no_child_and_choices_reach_cursor_with_muqun_tools() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch();
    let binary = root.0.join("agent");
    std::fs::write(&binary, FAKE_AGENT).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let driver = CursorDriver::new(
        binary.to_string_lossy().into_owned(),
        root.0.to_string_lossy().into_owned(),
        Arc::new(MemoryMirror::for_agent("cursor")),
        tokio::sync::broadcast::channel(256).0,
    );
    let muqun = json!([{"type":"http","name":"muqun","url":"http://127.0.0.1:1/mcp","headers":[]}]);
    driver.set_mcp_servers(muqun.clone());

    // Signed out, `authenticate` would open a browser: nothing may start.
    std::fs::write(root.0.join("signed-out"), b"").unwrap();
    match driver.create_session(None, None, None).await {
        Err(AgentError::NotAvailable(message)) => assert_eq!(message, NOT_SIGNED_IN),
        other => panic!("expected the sign-in refusal, got {other:?}"),
    }
    assert!(!root.0.join("requests.jsonl").exists(), "no ACP child ran");
    std::fs::remove_file(root.0.join("signed-out")).unwrap();

    let nano = ModelRef {
        provider_id: "cursor".into(),
        model_id: "gpt-5.4-nano[reasoning=medium]".into(),
        variant: None,
    };
    let info = driver
        .create_session(None, Some(&nano), None)
        .await
        .unwrap();
    let id = info.asid.0.clone();
    let catalog = driver
        .get_catalog_for_session(Some(&id), None)
        .await
        .unwrap();
    let models: Vec<_> = catalog
        .models
        .iter()
        .map(|model| model.name.as_str())
        .collect();
    assert_eq!(models, ["Auto", "gpt-5.4-nano"]);
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
    let methods: Vec<_> = requests
        .iter()
        .filter_map(|r| r["method"].as_str())
        .take(3)
        .collect();
    assert_eq!(methods, ["initialize", "authenticate", "session/new"]);
    let of = |method: &str| -> Vec<&Value> {
        requests.iter().filter(|r| r["method"] == method).collect()
    };
    assert_eq!(of("session/new")[0]["params"]["mcpServers"], muqun);
    assert!(of("session/set_config_option").iter().any(|r| {
        r["params"]["configId"] == "model"
            && r["params"]["value"] == "gpt-5.4-nano[reasoning=medium]"
    }));
    assert_eq!(of("session/prompt").len(), 1);
    driver.shutdown().await;
}

/// Real turns through `agent acp` on Cursor's included Auto model: a turn,
/// verified idle retirement, an offline listing, then a reload and a second
/// turn that remembers the first. Needs a signed-in Cursor CLI on PATH and
/// `CURSOR_ACP_TEST_WORKSPACE` (a throwaway repository).
#[tokio::test]
#[ignore = "runs real Cursor turns (Auto model)"]
async fn real_turn_retires_lists_and_reloads() {
    use crate::agents::domain::{AgentPart, RuntimeReason, SessionQuery};
    let cwd = std::env::var("CURSOR_ACP_TEST_WORKSPACE").expect("workspace");
    let binary = crate::agents::tasks::find_on_path("agent").expect("agent");
    let driver = CursorDriver::new(
        binary.to_string_lossy().into_owned(),
        cwd.clone(),
        Arc::new(MemoryMirror::for_agent("cursor")),
        tokio::sync::broadcast::channel(1024).0,
    );
    driver.set_idle_timeout(Duration::from_secs(1));
    let auto = ModelRef {
        provider_id: "cursor".into(),
        model_id: "default[]".into(),
        variant: None,
    };
    let info = driver
        .create_session(Some(&cwd), Some(&auto), None)
        .await
        .unwrap();
    let id = info.asid.0.clone();
    let idle = |driver: Arc<CursorDriver>, id: String| async move {
        tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let info = driver.get_session(&id).await.unwrap();
                if info.status == AgentSessionStatus::Idle {
                    break;
                }
                assert!(info.error.is_none(), "{:?}", info.error);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
    };
    driver
        .send_prompt(
            &id,
            "Remember the codeword ZEBRA. Reply with exactly OK",
            &[],
            None,
        )
        .await
        .unwrap();
    idle(driver.clone(), id.clone()).await;
    tokio::time::timeout(Duration::from_secs(100), async {
        loop {
            let info = driver.get_session(&id).await.unwrap();
            if info.runtime.and_then(|r| r.reason) == Some(RuntimeReason::NotLoaded) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .expect("idle child retired");
    assert!(!driver.has_live_stream());
    let listed = driver
        .list_sessions(&SessionQuery {
            directory: Some(cwd.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(listed.iter().any(|row| row.asid.0 == id));
    driver
        .send_prompt(
            &id,
            "What was the codeword? Reply with only the codeword.",
            &[],
            None,
        )
        .await
        .unwrap();
    idle(driver.clone(), id.clone()).await;
    let rows = driver.get_timeline(&id, 100).await.unwrap();
    let texts: Vec<_> = rows
        .iter()
        .filter_map(|row| match &row.part {
            AgentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text.contains("ZEBRA") && !text.contains("Remember")),
        "{texts:?}"
    );
    driver.shutdown().await;
}
