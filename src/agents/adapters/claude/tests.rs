//! The Claude driver against a fake adapter: listing starts nothing, the
//! launch reaches the child, and muqun MCP plus model/effort reach ACP.
use std::sync::Arc;

use serde_json::{json, Value};

use super::ClaudeDriver;
use crate::agents::adapters::acp::session::AcpLaunch;
use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::domain::{ModelRef, SessionQuery};
use crate::agents::ports::agent::{AgentError, AgentPort};

const FAKE: &str = r#"#!/usr/bin/env python3
import json, os, sys
def send(value): print(json.dumps(value), flush=True)
def log(name, value):
    with open(name, 'a') as f: f.write(json.dumps(value) + '\n')
log('launch.log', {'args': sys.argv[1:], 'protocol': os.environ.get('CLAUDE_AGENT_ACP_PROTOCOL')})
config = {'configOptions': [
    {'id': 'model', 'category': 'model', 'currentValue': 'default', 'options': [{'value': 'default', 'name': 'Default'}, {'value': 'haiku', 'name': 'Haiku'}]},
    {'id': 'effort', 'category': 'thought_level', 'currentValue': 'default', 'options': [{'value': 'default', 'name': 'Default'}, {'value': 'low', 'name': 'Low'}]}]}
for line in sys.stdin:
    message = json.loads(line)
    method, params = message.get('method'), message.get('params', {})
    log('methods.log', [method, params])
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True, 'sessionCapabilities': {'list': {}}}, 'authMethods': []}})
    elif method == 'session/new':
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': dict(config, sessionId='00000000-0000-4000-8000-0000000000aa')})
    elif method == 'session/set_config_option':
        for option in config['configOptions']:
            if option['id'] == params['configId']: option['currentValue'] = params['value']
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': config})
    elif method == 'session/prompt':
        send({'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32000, 'message': 'Authentication required'}})
    elif 'id' in message:
        send({'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32601, 'message': 'unsupported'}})
"#;

fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn listing_starts_nothing_and_sessions_get_launch_mcp_and_model() {
    use std::os::unix::fs::PermissionsExt;
    let directory =
        std::env::temp_dir().join(format!("muqun-claude-driver-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let fake = directory.join("fake-claude-agent-acp");
    std::fs::write(&fake, FAKE).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cwd = directory.to_str().unwrap().to_owned();
    let (tx, _) = tokio::sync::broadcast::channel(64);
    let driver: Arc<ClaudeDriver> = ClaudeDriver::new(
        fake.to_string_lossy().into_owned(),
        cwd.clone(),
        Arc::new(MemoryMirror::for_agent("claude")),
        tx,
    );
    driver.set_launch(AcpLaunch {
        args: vec!["--synthetic-runner-arg".into()],
        env: vec![("CLAUDE_AGENT_ACP_PROTOCOL".into(), "v1".into())],
    });
    let mcp = json!([{"type":"http","name":"muqun","url":"http://127.0.0.1:1/mcp",
                      "headers":[{"name":"Authorization","value":"Bearer synthetic"}]}]);
    driver.set_mcp_servers(mcp.clone());

    // No transcript bucket for this workspace: an empty list, and no child.
    let query = SessionQuery {
        directory: Some(cwd.clone()),
        ..Default::default()
    };
    assert!(driver.list_sessions(&query).await.unwrap().is_empty());
    // The permission modes are listed before any adapter runs.
    let modes: Vec<_> = driver
        .get_catalog(Some(&cwd))
        .await
        .unwrap()
        .modes
        .into_iter()
        .map(|mode| mode.id)
        .collect();
    assert_eq!(modes[..4], ["default", "acceptEdits", "plan", "auto"]);
    assert!(!directory.join("launch.log").exists());

    let model = ModelRef {
        provider_id: "claude".into(),
        model_id: "haiku".into(),
        variant: Some("low".into()),
    };
    let info = driver
        .create_session(Some(&cwd), Some(&model), None)
        .await
        .unwrap();
    assert_eq!(info.model.as_ref(), Some(&model));
    assert_eq!(
        lines(&directory.join("launch.log")),
        [json!({"args":["--synthetic-runner-arg"],"protocol":"v1"})]
    );
    let methods = lines(&directory.join("methods.log"));
    let new = methods.iter().find(|m| m[0] == "session/new").unwrap();
    assert_eq!(new[1]["mcpServers"], mcp);
    let configs: Vec<_> = methods
        .iter()
        .filter(|m| m[0] == "session/set_config_option")
        .map(|m| (m[1]["configId"].clone(), m[1]["value"].clone()))
        .collect();
    assert_eq!(
        configs,
        [
            (json!("model"), json!("haiku")),
            (json!("effort"), json!("low"))
        ]
    );

    // A prompt rejected for a missing login says so; nothing is retried.
    driver
        .send_prompt(&info.asid.0, "Reply with exactly PONG", &[], None)
        .await
        .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(error) = driver.get_session(&info.asid.0).await.unwrap().error {
                break error.message;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(error.contains("not signed in"), "{error}");
    let prompts = lines(&directory.join("methods.log"))
        .into_iter()
        .filter(|m| m[0] == "session/prompt")
        .count();
    assert_eq!(prompts, 1);

    // Hand-over: the same session is now resumed in a terminal pane. The
    // Gateway's idle child is released and input goes into that pane; no
    // second agent process receives the prompt.
    let bridge = Arc::new(FakeTerminal::default());
    driver.set_terminal_bridge(bridge.clone());
    driver
        .send_prompt(&info.asid.0, "Reply with exactly PONG", &[], None)
        .await
        .unwrap();
    assert_eq!(
        bridge.sent.lock().unwrap().as_slice(),
        [("pane-1".to_owned(), "Reply with exactly PONG".to_owned())]
    );
    assert!(!driver.has_live_stream());
    let runtime = driver.session_runtime(&info.asid.0).await.unwrap().unwrap();
    assert_eq!(
        runtime.interaction,
        crate::agents::domain::RuntimeInteraction::Terminal
    );
    let prompts = lines(&directory.join("methods.log"))
        .into_iter()
        .filter(|m| m[0] == "session/prompt")
        .count();
    assert_eq!(prompts, 1);

    // Every read of a terminal-held session answers from its transcript;
    // only what needs the conversation itself goes to the terminal (409),
    // and without a pane so do prompts and Stop.
    let id = info.asid.0.as_str();
    for pane in [true, false] {
        bridge.pane.store(pane, std::sync::atomic::Ordering::SeqCst);
        driver.prepare_observed_session(id).await.unwrap();
        driver.prepare_selected_session(id).await.unwrap();
        driver.get_timeline(id, 100).await.unwrap();
        driver.load_earlier_timeline(id, 50).await.unwrap();
        assert!(driver.get_pending_permissions(id).await.unwrap().is_empty());
        // Its mode belongs to the terminal: none is offered to switch.
        assert!(driver
            .get_catalog_for_session(Some(id), Some(&cwd))
            .await
            .unwrap()
            .modes
            .is_empty());
        let terminal = |result: Result<(), AgentError>| {
            matches!(result, Err(AgentError::TerminalInteraction(_)))
        };
        assert!(terminal(driver.switch_model(id, &model).await));
        assert!(terminal(driver.delete_session(id).await));
        assert!(terminal(driver.fork_session(id, None).await.map(|_| ())));
        assert_eq!(terminal(driver.interrupt(id).await), !pane);
        assert_eq!(
            terminal(
                driver
                    .send_prompt(id, "Reply with exactly PONG", &[], None)
                    .await
            ),
            !pane
        );
    }
    assert!(!driver.has_live_stream());
    driver.shutdown().await;
    let _ = std::fs::remove_dir_all(&directory);
}

struct FakeTerminal {
    sent: std::sync::Mutex<Vec<(String, String)>>,
    pane: std::sync::atomic::AtomicBool,
}

impl Default for FakeTerminal {
    fn default() -> Self {
        Self {
            sent: Default::default(),
            pane: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl crate::agents::adapters::acp::session::TerminalBridge for FakeTerminal {
    fn live<'a>(
        &'a self,
        ids: &'a [String],
    ) -> crate::agents::ports::agent::AgentFuture<
        'a,
        std::collections::HashMap<String, crate::agents::adapters::acp::session::TerminalLive>,
    > {
        Box::pin(async move {
            let process = super::native_sessions::LiveProcess {
                pid: 4242,
                start: "1".into(),
                busy: Some(false),
            };
            Ok(ids
                .iter()
                .map(|id| {
                    let pane = self
                        .pane
                        .load(std::sync::atomic::Ordering::SeqCst)
                        .then(|| ("terminal".to_owned(), "pane-1".to_owned()));
                    let live = crate::agents::runtime::claude::terminal_runtime(&process, pane);
                    (id.clone(), live)
                })
                .collect())
        })
    }
    fn send<'a>(
        &'a self,
        route: &'a crate::agents::adapters::acp::session::TerminalRoute,
        text: &'a str,
    ) -> crate::agents::ports::agent::AgentFuture<'a, ()> {
        self.sent
            .lock()
            .unwrap()
            .push((route.pane.clone(), text.to_owned()));
        Box::pin(async { Ok(()) })
    }
    fn interrupt<'a>(
        &'a self,
        _route: &'a crate::agents::adapters::acp::session::TerminalRoute,
    ) -> crate::agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// One real turn through the official adapter on the cheapest model, idle
/// retirement, a transcript listing and a reload. Needs a signed-in Claude
/// Code and `CLAUDE_ACP_TEST_ADAPTER` (the adapter's `dist/index.js`) and
/// `CLAUDE_ACP_TEST_WORKSPACE` (a throwaway repository).
#[tokio::test]
#[ignore = "runs real Claude Code turns (haiku, low effort)"]
async fn real_turn_retires_lists_and_reloads() {
    use crate::agents::domain::{AgentSessionStatus, RuntimeReason};
    use std::time::Duration;
    let adapter = std::env::var("CLAUDE_ACP_TEST_ADAPTER").expect("adapter path");
    let cwd = std::env::var("CLAUDE_ACP_TEST_WORKSPACE").expect("workspace");
    let claude = crate::agents::tasks::find_on_path("claude").expect("claude");
    let (tx, _) = tokio::sync::broadcast::channel(1024);
    let driver: Arc<ClaudeDriver> = ClaudeDriver::new(
        "node".into(),
        cwd.clone(),
        Arc::new(MemoryMirror::for_agent("claude")),
        tx,
    );
    driver.set_launch(AcpLaunch {
        args: vec![adapter],
        env: vec![
            ("CLAUDE_AGENT_ACP_PROTOCOL".into(), "v1".into()),
            (
                "CLAUDE_CODE_EXECUTABLE".into(),
                claude.to_string_lossy().into(),
            ),
        ],
    });
    driver.set_idle_timeout(Duration::from_secs(1));
    let model = ModelRef {
        provider_id: "claude".into(),
        model_id: "haiku".into(),
        variant: Some("low".into()),
    };
    let info = driver
        .create_session(Some(&cwd), Some(&model), None)
        .await
        .unwrap();
    let id = info.asid.0.clone();
    let idle = |driver: Arc<ClaudeDriver>, id: String| async move {
        tokio::time::timeout(Duration::from_secs(120), async {
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
        .send_prompt(&id, "Reply with exactly PONG", &[], None)
        .await
        .unwrap();
    idle(driver.clone(), id.clone()).await;
    // Retirement runs on the 30 s tick once the native transcript proves the turn.
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
        .send_prompt(&id, "Reply with exactly PONG", &[], None)
        .await
        .unwrap();
    idle(driver.clone(), id.clone()).await;
    let rows = driver.get_timeline(&id, 100).await.unwrap();
    let texts: Vec<_> = rows
        .iter()
        .filter_map(|row| match &row.part {
            crate::agents::domain::AgentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().filter(|text| text.contains("PONG")).count() >= 2,
        "{texts:?}"
    );
    driver.shutdown().await;
}

/// Modes, the task instruction, a question, steering, rename and delete
/// after a Gateway restart, each through this driver against the real
/// adapter (same environment as `real_turn_retires_lists_and_reloads`).
#[tokio::test]
#[ignore = "runs real Claude Code turns (haiku, low effort)"]
async fn real_modes_instruction_question_steer_rename_and_owned_delete() {
    use crate::agents::domain::{AgentPart, AgentSessionStatus, TimelineRole};
    use std::time::Duration;
    let adapter = std::env::var("CLAUDE_ACP_TEST_ADAPTER").expect("adapter path");
    let cwd = std::env::var("CLAUDE_ACP_TEST_WORKSPACE").expect("workspace");
    let claude = crate::agents::tasks::find_on_path("claude").expect("claude");
    let store = std::env::temp_dir().join(format!("muqun-claude-owned-{}", uuid::Uuid::new_v4()));
    let start = || {
        let driver: Arc<ClaudeDriver> = ClaudeDriver::new(
            "node".into(),
            cwd.clone(),
            Arc::new(MemoryMirror::for_agent("claude")),
            tokio::sync::broadcast::channel(4096).0,
        );
        driver.set_launch(AcpLaunch {
            args: vec![adapter.clone()],
            env: vec![
                ("CLAUDE_AGENT_ACP_PROTOCOL".into(), "v1".into()),
                (
                    "CLAUDE_CODE_EXECUTABLE".into(),
                    claude.to_string_lossy().into(),
                ),
            ],
        });
        driver.set_owned_store(store.clone());
        driver
    };
    let driver = start();
    let wait = |driver: Arc<ClaudeDriver>, id: String, busy: bool| async move {
        tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let info = driver.get_session(&id).await.unwrap();
                assert!(info.error.is_none(), "{:?}", info.error);
                if (info.status == AgentSessionStatus::Idle) != busy {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
    };
    let texts = |rows: &[crate::agents::domain::TimelineItem], role: TimelineRole| -> Vec<String> {
        rows.iter()
            .filter(|row| row.role == role)
            .filter_map(|row| match &row.part {
                AgentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    let model = ModelRef {
        provider_id: "claude".into(),
        model_id: "haiku".into(),
        variant: Some("low".into()),
    };
    let info = driver
        .create_session(Some(&cwd), Some(&model), Some("acceptEdits"))
        .await
        .unwrap();
    let id = info.asid.0.clone();
    assert_eq!(info.mode.as_deref(), Some("acceptEdits"));
    let catalog = driver
        .get_catalog_for_session(Some(&id), Some(&cwd))
        .await
        .unwrap();
    assert!(
        catalog.modes.iter().any(|mode| mode.id == "plan"),
        "{:?}",
        catalog.modes
    );
    driver.switch_mode(&id, "plan").await.unwrap();
    assert_eq!(
        driver.get_session(&id).await.unwrap().mode.as_deref(),
        Some("plan")
    );
    driver.switch_mode(&id, "default").await.unwrap();
    // Claude Code may change the model around plan mode; choose it again.
    driver.switch_model(&id, &model).await.unwrap();

    // The task sentence: appended to the system prompt; the session keeps
    // its model and mode across the restart that applies it.
    assert!(driver
        .set_session_instruction(
            &id,
            "muqun-task",
            "Always end every reply with the word KIWI on its own line."
        )
        .await
        .unwrap());
    let now = driver.get_session(&id).await.unwrap();
    assert_eq!(now.model.as_ref(), Some(&model));
    assert_eq!(now.mode.as_deref(), Some("default"));
    driver
        .send_prompt(&id, "Say hello.", &[], None)
        .await
        .unwrap();
    wait(driver.clone(), id.clone(), false).await;
    let rows = driver.get_timeline(&id, 200).await.unwrap();
    let said = texts(&rows, TimelineRole::Assistant);
    assert!(said.last().unwrap().contains("KIWI"), "{said:?}");
    driver
        .remove_session_instruction(&id, "muqun-task")
        .await
        .unwrap();

    // A question: AskUserQuestion becomes the App form, the answer returns.
    driver
        .send_prompt(&id, "Use the AskUserQuestion tool once to ask me to pick a fruit: Apple or Banana. Then reply with only the fruit I picked.", &[], None)
        .await
        .unwrap();
    let form = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Some(form) = driver.get_pending_forms(&id).await.unwrap().pop() {
                break form;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let key = match &form.fields[0] {
        crate::agents::domain::FormField::String { key, .. } => key.clone(),
        other => panic!("{other:?}"),
    };
    driver
        .reply_form(&id, &form.id, json!({ key: "Banana" }))
        .await
        .unwrap();
    wait(driver.clone(), id.clone(), false).await;
    let rows = driver.get_timeline(&id, 200).await.unwrap();
    assert!(texts(&rows, TimelineRole::Assistant)
        .last()
        .unwrap()
        .contains("Banana"));

    // Steering: a prompt during a turn joins it; one turn ends.
    driver
        .send_prompt(
            &id,
            "Write the numbers from 1 to 200, one per line, nothing else.",
            &[],
            None,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let rows = driver.get_timeline(&id, 200).await.unwrap();
            if texts(&rows, TimelineRole::Assistant)
                .last()
                .is_some_and(|t| t.contains("\n3"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let steer = "Stop counting now and reply with exactly PONG.";
    driver
        .send_prompt(&id, steer, &[], Some("steer"))
        .await
        .unwrap();
    wait(driver.clone(), id.clone(), false).await;
    let rows = driver.get_timeline(&id, 300).await.unwrap();
    assert!(texts(&rows, TimelineRole::User).iter().any(|t| t == steer));
    assert!(texts(&rows, TimelineRole::Assistant)
        .last()
        .unwrap()
        .contains("PONG"));
    assert!(matches!(
        driver.send_prompt(&id, "x", &[], Some("queue")).await,
        Err(AgentError::Unsupported(_))
    ));

    // Rename: Claude Code's own command; no rows, the title moves.
    let before = driver.get_timeline(&id, 300).await.unwrap().len();
    driver.rename_session(&id, "Parity spike").await.unwrap();
    assert_eq!(driver.get_session(&id).await.unwrap().title, "Parity spike");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(driver.get_timeline(&id, 300).await.unwrap().len(), before);

    // Delete after a restart: the new run still knows it created the session.
    driver.shutdown().await;
    let restarted = start();
    restarted.delete_session(&id).await.unwrap();
    let listed = restarted
        .list_sessions(&SessionQuery {
            directory: Some(cwd.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(listed.iter().all(|row| row.asid.0 != id));
    restarted.shutdown().await;
    let _ = std::fs::remove_file(store);
}
