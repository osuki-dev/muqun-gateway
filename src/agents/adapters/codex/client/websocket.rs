//! The shared daemon owns its process and threads. Cancellation drops only this socket.
use super::*;
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

impl CodexClient {
    pub(crate) fn from_websocket(
        mut socket: WebSocketStream<UnixStream>,
        home: std::path::PathBuf,
        shared_socket: std::path::PathBuf,
    ) -> (Arc<Self>, mpsc::Receiver<CodexEvent>) {
        let (events, rx) = mpsc::channel(QUEUE_SIZE);
        let (outgoing, mut writes) = mpsc::channel::<WritePacket>(MAX_PENDING);
        let state = Arc::new(State {
            generation: uuid::Uuid::new_v4().to_string(),
            shared: true,
            subscribed: Mutex::new(HashMap::new()),
            ownership_revision: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            completed_turns: Mutex::new(HashMap::new()),
            lifecycle_completed_turns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ready: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            events,
        });
        let pump = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = pump.cancel.cancelled() => break,
                    packet = writes.recv() => {
                        let Some(packet) = packet else { break; };
                        // The stdio encoder adds a newline; WebSocket carries one JSON message.
                        let mut text = String::from_utf8(packet.bytes).expect("JSON is UTF-8");
                        text.pop();
                        let result = tokio::select! {
                            biased;
                            _ = pump.cancel.cancelled() => Err(disconnected()),
                            result = tokio::time::timeout(REQUEST_TIMEOUT, socket.send(Message::Text(text.into()))) => {
                                result.ok().and_then(Result::ok).map_or_else(|| Err(disconnected()), |_| Ok(()))
                            }
                        };
                        let failed = result.is_err();
                        let _ = packet.done.send(result);
                        if failed { break; }
                    }
                    frame = socket.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) if text.len() <= MAX_FRAME_BYTES => {
                                let Ok(value) = serde_json::from_str(&text) else { break; };
                                if route_frame(&pump, value, text.len()).is_err() { break; }
                            }
                            Some(Ok(Message::Ping(_))) => {
                                // tungstenite queues the matching pong while reading.
                                let result = tokio::select! {
                                    _ = pump.cancel.cancelled() => break,
                                    result = tokio::time::timeout(REQUEST_TIMEOUT, socket.flush()) => result,
                                };
                                if !matches!(result, Ok(Ok(()))) { break; }
                            }
                            Some(Ok(Message::Pong(_))) => {}
                            _ => break,
                        }
                    }
                }
            }
            pump.stop();
            let _ = pump.events.try_send(CodexEvent::Disconnected);
            // No daemon shutdown RPC, signal, unsubscribe, or lifecycle command.
        });
        (
            Arc::new(Self {
                state,
                outgoing,
                timeout: REQUEST_TIMEOUT,
                config_cwd: None,
                configured_provider: std::sync::OnceLock::new(),
                expected_home: Some(home),
                codex_home: std::sync::OnceLock::new(),
                shared_socket: Some(shared_socket),
            }),
            rx,
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agents::adapters::codex::daemon::tests::TestDirectory;
    use crate::agents::adapters::codex::CodexDriver;
    use crate::agents::domain::RuntimeSource;
    use crate::agents::ports::agent::AgentPort;

    pub(crate) struct Fixture {
        pub(crate) client: Arc<CodexClient>,
        pub(crate) events: mpsc::Receiver<CodexEvent>,
        requests: Arc<Mutex<Vec<Value>>>,
        pub(crate) send: mpsc::Sender<Value>,
        disconnected: Arc<AtomicBool>,
        task: tokio::task::JoinHandle<()>,
        _home: TestDirectory,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.client.stop();
            self.task.abort();
        }
    }
    impl Fixture {
        pub(crate) async fn new(handler: impl Fn(&Value) -> Value + Send + 'static) -> Self {
            let home = TestDirectory::new();
            let (left, right) = UnixStream::pair().unwrap();
            let (client, server) = tokio::join!(
                tokio_tungstenite::client_async("ws://localhost/rpc", left),
                tokio_tungstenite::accept_async(right),
            );
            let (socket, _) = client.unwrap();
            let mut server = server.unwrap();
            let (client, events) =
                CodexClient::from_websocket(socket, home.0.clone(), home.0.join("fixture.sock"));
            let home_path = home.0.clone();
            let requests = Arc::new(Mutex::new(vec![]));
            let seen = requests.clone();
            let disconnected = Arc::new(AtomicBool::new(false));
            let closed = disconnected.clone();
            let (send, mut outbound) = mpsc::channel::<Value>(32);
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        message = outbound.recv() => {
                            let Some(message) = message else { break; };
                            if server.send(Message::Text(message.to_string().into())).await.is_err() { break; }
                        }
                        message = server.next() => {
                            let Some(Ok(Message::Text(text))) = message else { break; };
                            let request: Value = serde_json::from_str(&text).unwrap();
                            locked(&seen).push(request.clone());
                            let Some(id) = request.get("id") else { continue; };
                            let result = match request["method"].as_str() {
                                Some("initialize") => json!({"userAgent":"codex-tui/0.162.1", "codexHome":home_path}),
                                Some("account/read") => json!({"requiresOpenaiAuth":false,"account":null}),
                                _ => handler(&request),
                            };
                            if server.send(Message::Text(json!({"id":id,"result":result}).to_string().into())).await.is_err() { break; }
                        }
                    }
                }
                closed.store(true, Ordering::Release);
            });
            client.initialize().await.unwrap();
            Self {
                client,
                events,
                requests,
                send,
                disconnected,
                task,
                _home: home,
            }
        }
    }

    #[tokio::test]
    async fn shared_daemon_rejoins_only_requested_loaded_timeline_and_filters_other_threads() {
        let mut fixture = Fixture::new(|request| {
            let id = request["params"]["threadId"].as_str().unwrap_or("live");
            let thread = json!({"id":id,"historyMode":"paginated","status":{"type":if id=="live" {"active"} else {"notLoaded"},"activeFlags":[]}});
            match request["method"].as_str().unwrap() {
                "thread/read" | "thread/resume" => json!({"thread":thread}),
                "thread/items/list" => json!({"data":[],"nextCursor":null}),
                "thread/turns/list" => json!({"data":[{"id":"active-turn","status":"inProgress","items":[]}],"nextCursor":null}),
                other => panic!("unexpected RPC {other}"),
            }
        }).await;
        let driver = CodexDriver::new(fixture.client.clone());
        driver.get_session("live").await.unwrap();
        driver.get_timeline("external", 10).await.unwrap();
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "thread/resume"));
        driver.get_timeline("live", 10).await.unwrap();
        driver.get_timeline("live", 10).await.unwrap();
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/resume")
                .count(),
            1
        );
        assert!(locked(&fixture.requests)
            .iter()
            .filter(|r| r["method"] == "thread/resume")
            .all(|r| r["params"] == json!({"threadId":"live","excludeTurns":true})));
        assert_eq!(
            fixture.client.active_turn("live").as_deref(),
            Some("active-turn")
        );
        assert_eq!(
            driver.runtime("live", false).await.unwrap().source,
            RuntimeSource::Native
        );
        fixture.send.send(json!({"method":"turn/started","params":{"threadId":"unrequested","turn":{"id":"hidden"}}})).await.unwrap();
        fixture.send.send(json!({"id":11,"method":"item/tool/requestUserInput","params":{"threadId":"unrequested","turnId":"hidden"}})).await.unwrap();
        fixture.send.send(json!({"id":12,"method":"item/tool/requestUserInput","params":{"threadId":"live","turnId":"active-turn"}})).await.unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), fixture.events.recv())
                .await
                .unwrap()
                .unwrap();
            if let CodexEvent::Request { id, params, .. } = event {
                assert_eq!(id, 12);
                assert_eq!(params["threadId"], "live");
                break;
            }
        }
        assert!(fixture.client.active_turn("unrequested").is_none());
        assert!(fixture.client.server_requests("unrequested").is_empty());
        assert_eq!(fixture.client.server_requests("live").len(), 1);
        fixture.send.send(json!({"method":"turn/completed","params":{"threadId":"live","turn":{"id":"active-turn","status":"completed"}}})).await.unwrap();
        loop {
            if matches!(tokio::time::timeout(Duration::from_secs(2), fixture.events.recv()).await.unwrap().unwrap(), CodexEvent::Notification { method, .. } if method=="turn/completed")
            {
                break;
            }
        }
        fixture
            .client
            .observe_active_turn("live", &json!({"id":"active-turn","status":"inProgress"}));
        assert!(
            fixture.client.active_turn("live").is_none(),
            "a late read cannot resurrect a completed turn"
        );
    }

    #[tokio::test]
    async fn shared_rejoin_initial_status_replaces_unknown_and_completion_wins() {
        use crate::agents::domain::{AgentDomainEvent, AgentSessionId, AgentSessionStatus};
        use crate::agents::manager::AgentManager;
        let mut fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" | "thread/resume" => json!({"thread":{"id":"live","historyMode":"paginated","status":{"type":"active","activeFlags":[]}}}),
            "thread/turns/list" => json!({"data":[{"id":"active-turn","status":"inProgress"}]}),
            "thread/items/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("unexpected RPC {other}"),
        }).await;
        let (tx, mut updates) = tokio::sync::broadcast::channel(64);
        let manager = AgentManager::connect_codex_observed(
            fixture.client.clone(),
            std::mem::replace(&mut fixture.events, mpsc::channel(1).1),
            tx,
            Some(crate::agents::terminal_runtime::TerminalRuntime::new(&[])),
        );
        assert_eq!(
            manager.sessions().get_session("live").await.unwrap().status,
            AgentSessionStatus::Unknown
        );
        let asid = AgentSessionId("live".into());
        manager.sessions().get_snapshot(&asid).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(updates.recv().await.unwrap(), AgentDomainEvent::SessionUpdated { info, .. } if info.status == AgentSessionStatus::Busy && info.runtime.as_ref().is_some_and(|r| r.source == RuntimeSource::Native)) {
                    break;
                }
            }
        }).await.unwrap();
        fixture.send.send(json!({"method":"turn/completed","params":{"threadId":"live","turn":{"id":"active-turn","status":"completed"}}})).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    updates.recv().await.unwrap(),
                    AgentDomainEvent::StatusChanged {
                        status: AgentSessionStatus::Idle,
                        ..
                    }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager
                .sessions()
                .get_snapshot(&asid)
                .await
                .unwrap()
                .info
                .status,
            AgentSessionStatus::Idle
        );
    }

    #[tokio::test]
    async fn shared_terminal_start_keeps_context_in_thread_and_returns_exact_remote_resume() {
        let fixture = Fixture::new(|request| {
            assert_eq!(request["method"], "thread/start");
            assert_eq!(request["params"]["developerInstructions"], "Synthetic owner instructions. Synthetic task completion rule.");
            assert_eq!(request["params"]["config"]["mcp_servers.muqun.http_headers_helper"], "fixture-helper --task-id synthetic-task");
            assert_eq!(request["params"]["cwd"], "/synthetic/project");
            json!({"thread":{"id":"00000000-0000-4000-8000-000000000001","historyMode":"paginated","status":{"type":"idle"}}})
        }).await;
        let driver = CodexDriver::new(fixture.client.clone());
        let arguments = ["-c", "projects={ '/synthetic/project' = { trust_level = 'trusted' } }", "-c", "developer_instructions='Synthetic owner instructions. Synthetic task completion rule.'", "-c", "mcp_servers.muqun.http_headers_helper='fixture-helper --task-id synthetic-task'"]
            .into_iter().map(str::to_owned).collect::<Vec<_>>();
        let prepared = driver
            .prepare_terminal_launch("/synthetic/project", &arguments)
            .await
            .unwrap();
        assert_eq!(
            prepared.arguments,
            [
                "--remote".to_owned(),
                format!("unix://{}", fixture._home.0.join("fixture.sock").display()),
                "resume".into(),
                prepared.session.asid.0.clone()
            ]
        );
        assert!(fixture.client.is_subscribed(&prepared.session.asid.0));
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "turn/start"));
        assert!(matches!(
            driver
                .prepare_terminal_launch("/synthetic/project", &["--no-daemon".into()])
                .await,
            Err(AgentError::Unsupported(_))
        ));
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/start")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn shared_task_launcher_submits_once_and_stops_after_cancellation_or_uncertain_creation()
    {
        use crate::agents::manager::AgentManager;
        use crate::agents::session_routes::{
            start_agent_in_place, type_agent_prompt, PlacedAgent, PromptDelivery, SpawnPlace,
            TaskLaunch,
        };
        use crate::tasks::domain::Task;
        for (cancelled, invalid_response, invalid_task) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let task_id = if invalid_task {
                crate::tasks::domain::TaskId("invalid".into())
            } else {
                crate::tasks::domain::TaskId::new()
            };
            let expected_task_id = task_id.0.clone();
            let mut fixture = Fixture::new(move |request| {
                assert_eq!(request["method"], "thread/start");
                let helper = request["params"]["config"]["mcp_servers.muqun.http_headers_helper"].as_str().unwrap();
                assert!(helper.ends_with(&format!("--task-id '{expected_task_id}'")));
                assert!(!helper.contains("--pid"));
                assert!(!helper.contains("synthetic-admin"));
                assert!(request["params"]["developerInstructions"].as_str().unwrap().contains("Synthetic owner instructions."));
                assert!(request["params"].get("input").is_none());
                if invalid_response { return json!({"thread":{"id":"invalid-returned-id","status":{"type":"idle"}}}); }
                json!({"thread":{"id":"00000000-0000-4000-8000-000000000001","historyMode":"paginated","status":{"type":"idle"}}})
            }).await;
            let herdr = crate::test_support::FakeHerdr::start(vec![], None);
            let mut state = crate::test_support::test_state("synthetic-admin", vec![]);
            state.config.sessions = vec![herdr.session()];
            state.config.mcp.inject_into_agents = true;
            let cwd = dirs::home_dir().unwrap().join(format!(
                ".muqun-shared-start-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir(&cwd).unwrap();
            let _cwd = TestDirectory(cwd.clone());
            state.config.agent_launch_args.insert(
                "codex".into(),
                vec![
                    "-c".into(),
                    format!(
                        "projects={}",
                        toml::Value::try_from(
                            json!({cwd.to_str().unwrap(): {"trust_level":"trusted"}})
                        )
                        .unwrap()
                    ),
                    "-c".into(),
                    "developer_instructions=\"Synthetic owner instructions.\"".into(),
                ],
            );
            let task: Task = serde_json::from_value(json!({
                "id":task_id.0, "kind":"delegation", "goal":"synthetic goal", "cwd":cwd,
                "binding":{"kind":"pane","session_id":"default","pane_id":"w1:p1","tab_id":"w1:t1","agent_id":"codex"},
                "state":if cancelled {"cancelled"} else {"dispatching"},
                "created_by":{"kind":"assistant","id":"synthetic"}, "created_ms":1,"updated_ms":1,"seq":1,"origin":"mcp"
            })).unwrap();
            state.tasks.insert_for_test(&task).await;
            let (tx, _) = tokio::sync::broadcast::channel(64);
            let manager = AgentManager::connect_codex(
                fixture.client.clone(),
                std::mem::replace(&mut fixture.events, mpsc::channel(1).1),
                tx,
            );
            state.agent_runtime.attach_for_test(Arc::new(manager)).await;
            let placed = PlacedAgent {
                place: SpawnPlace {
                    pane_id: "w1:p1".into(),
                    tab_id: Some("w1:t1".into()),
                    cwd: Some(cwd.clone()),
                    env_applied: true,
                },
                payload: json!({"agent_started":false,"prompt_submitted":false}),
                steps: crate::agents::tasks::StepLog::new(),
            };
            let (mut launch, to_type) = start_agent_in_place(
                &state,
                &herdr.session(),
                "codex",
                cwd.to_str(),
                placed,
                Some("synthetic goal"),
                Some(TaskLaunch {
                    task_id: &task.id.0,
                    out_of_band: true,
                    model_args: &[],
                }),
            )
            .await;
            if let Some(started) = to_type {
                assert!(!cancelled && !invalid_response && !invalid_task);
                type_agent_prompt(
                    &herdr.session(),
                    &started,
                    Some("synthetic goal"),
                    &mut launch,
                )
                .await;
            }
            assert!(!launch.steps.value().to_string().contains("synthetic-admin"));
            let calls = herdr.calls.lock().unwrap();
            assert!(!serde_json::to_string(&*calls)
                .unwrap()
                .contains("synthetic-admin"));
            let starts: Vec<_> = calls
                .iter()
                .filter(|r| r["method"] == "agent.start")
                .collect();
            assert_eq!(
                starts.len(),
                usize::from(!cancelled && !invalid_response && !invalid_task)
            );
            if cancelled || invalid_response || invalid_task {
                assert_eq!(launch.prompt_delivery, PromptDelivery::NotAttempted);
                assert_eq!(launch.payload["prompt_submitted"], false);
            } else {
                assert_eq!(launch.prompt_delivery, PromptDelivery::Attempted);
                assert_eq!(
                    starts[0]["params"]["args"],
                    json!([
                        "--remote",
                        format!("unix://{}", fixture._home.0.join("fixture.sock").display()),
                        "resume",
                        "00000000-0000-4000-8000-000000000001"
                    ])
                );
                assert_eq!(launch.payload["prompt_submitted"], true);
            }
            let prompts: Vec<_> = calls
                .iter()
                .filter(|r| r["method"] == "agent.prompt")
                .collect();
            assert_eq!(
                prompts.len(),
                usize::from(!cancelled && !invalid_response && !invalid_task)
            );
            if let Some(prompt) = prompts.first() {
                assert_eq!(prompt["params"]["text"], "synthetic goal");
            }
            assert!(!calls
                .iter()
                .any(|r| r["method"] == "pane.send_text" || r["method"] == "pane.focus"));
            assert_eq!(
                locked(&fixture.requests)
                    .iter()
                    .filter(|r| r["method"] == "thread/start")
                    .count(),
                usize::from(!invalid_task)
            );
        }
    }

    #[tokio::test]
    async fn shared_compaction_stops_after_subscription_is_lost_without_rejoining() {
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/start" | "thread/read" => {
                json!({"thread":{"id":"live","status":{"type":"idle"}}})
            }
            "thread/compact/start" => json!({}),
            other => panic!("compaction must not call {other}"),
        })
        .await;
        let driver = CodexDriver::new(fixture.client.clone());
        driver.create_session(None, None, None).await.unwrap();
        driver.compact_session("live", None).await.unwrap();
        fixture
            .send
            .send(json!({"method":"thread/closed","params":{"threadId":"live"}}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.client.is_subscribed("live") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            driver.compact_session("live", None).await,
            Err(AgentError::InvalidRequest(_))
        ));
        let requests = locked(&fixture.requests);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["method"] == "thread/compact/start")
                .count(),
            1
        );
        assert!(!requests
            .iter()
            .any(|request| request["method"] == "thread/resume"));
    }

    #[tokio::test]
    async fn shared_daemon_disconnect_sends_no_lifecycle_or_thread_mutation() {
        let fixture = Fixture::new(|_| panic!("disconnect must not make an RPC")).await;
        assert!(fixture.client.is_shared());
        assert_eq!(fixture.client.endpoint(), "unix://shared-codex-app-server");
        assert_eq!(fixture.client.codex_home(), Some(fixture._home.0.as_path()));
        fixture.client.stop();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.disconnected.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(locked(&fixture.requests).iter().all(|r| matches!(
            r["method"].as_str(),
            Some("initialize" | "initialized" | "account/read")
        )));
        assert!(!fixture.client.is_ready());
    }

    #[tokio::test]
    async fn shared_daemon_explicit_interrupt_rejoins_the_existing_active_turn() {
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" | "thread/resume" => json!({"thread":{"id":"live","historyMode":"paginated","status":{"type":"active","activeFlags":[]}}}),
            "thread/turns/list" => json!({"data":[{"id":"existing-turn","status":"inProgress"}]}),
            "turn/interrupt" => json!({}),
            other => panic!("unexpected RPC {other}"),
        }).await;
        let driver = CodexDriver::new(fixture.client.clone());
        driver.interrupt("live").await.unwrap();
        let requests = locked(&fixture.requests);
        let interrupt = requests
            .iter()
            .find(|r| r["method"] == "turn/interrupt")
            .unwrap();
        assert_eq!(
            interrupt["params"],
            json!({"threadId":"live","turnId":"existing-turn"})
        );
        assert!(!requests.iter().any(|r| r["method"] == "turn/start"));
    }

    #[tokio::test]
    async fn shared_daemon_does_not_resume_a_thread_unloaded_while_waiting_to_rejoin() {
        let reads = AtomicU64::new(0);
        let fixture = Fixture::new(move |request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                let status = if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                    "active"
                } else {
                    "notLoaded"
                };
                json!({"thread":{"id":"live","historyMode":"paginated","status":{"type":status}}})
            }
            "thread/items/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("unexpected RPC {other}"),
        })
        .await;
        let driver = CodexDriver::new(fixture.client.clone());
        driver.get_timeline("live", 10).await.unwrap();
        assert!(!fixture.client.is_subscribed("live"));
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "thread/resume"));
    }

    #[tokio::test]
    async fn shared_daemon_rejects_wrong_home_without_finishing_initialize() {
        let home = TestDirectory::new();
        let (left, right) = UnixStream::pair().unwrap();
        let (client, server) = tokio::join!(
            tokio_tungstenite::client_async("ws://localhost/rpc", left),
            tokio_tungstenite::accept_async(right)
        );
        let (client, _) = CodexClient::from_websocket(
            client.unwrap().0,
            home.0.clone(),
            home.0.join("fixture.sock"),
        );
        let mut server = server.unwrap();
        let task = tokio::spawn(async move {
            let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                panic!()
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            server.send(Message::Text(json!({"id":request["id"],"result":{"userAgent":"codex-tui/0.162.1","codexHome":"/"}}).to_string().into())).await.unwrap();
            assert!(!matches!(server.next().await, Some(Ok(Message::Text(_)))));
        });
        assert!(client
            .initialize()
            .await
            .unwrap_err()
            .to_string()
            .contains("home mismatch"));
        task.await.unwrap();
    }
}
