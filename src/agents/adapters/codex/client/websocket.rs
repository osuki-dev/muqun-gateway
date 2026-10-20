//! The shared daemon owns its process and threads. Cancellation drops only this socket.
use super::*;
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

fn socket_error_reason(
    state: &State,
    phase: &'static str,
    error: &tokio_tungstenite::tungstenite::Error,
) -> &'static str {
    use tokio_tungstenite::tungstenite::{error::CapacityError, Error};
    // Error Display/Debug can include frame contents or close text. Only fixed
    // categories, numeric bounds and OS error kinds may enter the journal.
    match error {
        Error::Capacity(CapacityError::MessageTooLong { size, max_size }) => {
            if state.history_only && phase == "receive" {
                // The decoder may have accumulated one bounded message and
                // read one further bounded frame before rejecting their sum.
                // A larger advertised frame is rejected before its body is read.
                let _ = state.history_capacity.set(HistoryCapacity {
                    wire_work: 2 * state.read_limit,
                });
            }
            tracing::warn!(connection = %state.generation, phase, frame_bytes = size, limit_bytes = max_size,
                "Codex WebSocket size limit exceeded");
            "websocket_size_limit"
        }
        Error::Io(error) => {
            tracing::warn!(connection = %state.generation, phase, io_kind = ?error.kind(),
                "Codex WebSocket I/O failed");
            "websocket_io_error"
        }
        Error::ConnectionClosed | Error::AlreadyClosed => "websocket_closed",
        Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => "websocket_peer_eof_without_close",
        Error::Protocol(_) => "websocket_protocol_error",
        Error::Utf8(_) => "websocket_utf8_error",
        _ => "websocket_other_error",
    }
}

fn router_error_reason(error: &AgentError) -> &'static str {
    // Match fixed local invariants, never print arbitrary error text.
    match error {
        AgentError::Protocol(message) => match message.as_str() {
            "Codex response exceeds allowed frame limit" => "rpc_router_frame_limit",
            "invalid Codex request id" => "rpc_router_invalid_callback_id",
            "Codex server request capacity or duplicate id" => {
                "rpc_router_callback_capacity_or_duplicate"
            }
            "Codex lifecycle completed-turn capacity exhausted" => "rpc_router_lifecycle_capacity",
            "Codex completed-turn capacity exhausted" => "rpc_router_completed_capacity",
            "invalid Codex RPC response envelope" => "rpc_router_invalid_response_envelope",
            "Codex attach response missing identity" => "rpc_router_missing_attach_identity",
            "Codex rejoin identity mismatch" => "rpc_router_rejoin_identity_mismatch",
            "Codex subscription capacity exhausted" => "rpc_router_subscription_capacity",
            "invalid Codex RPC envelope" => "rpc_router_invalid_envelope",
            _ => "rpc_router_protocol_error",
        },
        _ => "rpc_router_error",
    }
}

impl CodexClient {
    #[cfg(test)]
    pub(crate) fn from_websocket(
        socket: WebSocketStream<UnixStream>,
        home: std::path::PathBuf,
        shared_socket: std::path::PathBuf,
    ) -> (Arc<Self>, CodexEvents) {
        Self::from_websocket_with_parent(
            socket,
            home,
            shared_socket,
            None,
            MAX_SHARED_READ_FRAME_BYTES,
        )
    }

    pub(crate) fn from_websocket_with_parent(
        mut socket: WebSocketStream<UnixStream>,
        home: std::path::PathBuf,
        shared_socket: std::path::PathBuf,
        parent: Option<CancellationToken>,
        read_limit: usize,
    ) -> (Arc<Self>, CodexEvents) {
        let (events, rx) = events::channel(QUEUE_SIZE);
        let (outgoing, mut writes) = mpsc::channel::<WritePacket>(MAX_PENDING);
        let state = Arc::new(State {
            generation: uuid::Uuid::new_v4().to_string(),
            shared: true,
            history_only: parent.is_some(),
            read_limit,
            subscribed: Mutex::new(HashMap::new()),
            ownership_revision: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            requests: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            completed_turns: Mutex::new(HashMap::new()),
            lifecycle_completed_turns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ready: AtomicBool::new(false),
            disconnect_reason: std::sync::OnceLock::new(),
            history_capacity: std::sync::OnceLock::new(),
            cancel: parent.map_or_else(CancellationToken::new, |parent| parent.child_token()),
            events,
        });
        let pump = state.clone();
        tokio::spawn(async move {
            let reason = loop {
                tokio::select! {
                    biased;
                    _ = pump.cancel.cancelled() => break "connection_cancelled",
                    packet = writes.recv() => {
                        let Some(packet) = packet else { break "outgoing_queue_closed"; };
                        // The stdio encoder adds a newline; WebSocket carries one JSON message.
                        let mut text = String::from_utf8(packet.bytes).expect("JSON is UTF-8");
                        text.pop();
                        let result = tokio::select! {
                            biased;
                            _ = pump.cancel.cancelled() => Err("connection_cancelled"),
                            result = tokio::time::timeout(REQUEST_TIMEOUT, socket.send(Message::Text(text.into()))) => {
                                match result {
                                    Ok(Ok(())) => Ok(()),
                                    Ok(Err(error)) => Err(socket_error_reason(&pump, "send", &error)),
                                    Err(_) => Err("websocket_send_deadline"),
                                }
                            }
                        };
                        if let Err(reason) = result {
                            // Record the real I/O cause before waking a writer
                            // whose mutation guard also fences a failed send.
                            pump.stop_reason(reason);
                            let _ = packet.done.send(Err(disconnected()));
                            break reason;
                        }
                        let _ = packet.done.send(Ok(()));
                    }
                    frame = socket.next() => {
                        match frame {
                            Some(Ok(Message::Text(text))) if text.len() <= pump.read_limit => {
                                // Keep the small-frame fail-fast path unless an
                                // image-bearing item-page read is actually pending.
                                if text.len() > MAX_FRAME_BYTES && !locked(&pump.pending).values().any(|pending| pending.diagnostic_method == "thread/items/list" || (pump.history_only && matches!(pending.diagnostic_method, "thread/read" | "thread/turns/list"))) {
                                    tracing::warn!(connection = %pump.generation, frame_bytes = text.len(), limit_bytes = MAX_FRAME_BYTES,
                                        "Codex WebSocket size limit exceeded");
                                    break "websocket_size_limit";
                                }
                                let Ok(value) = serde_json::from_str(&text) else {
                                    tracing::warn!(connection = %pump.generation, frame_bytes = text.len(), "Codex WebSocket invalid JSON");
                                    break "websocket_invalid_json";
                                };
                                let frame_bytes = text.len();
                                drop(text);
                                if let Err(error) = route_frame(&pump, value, frame_bytes) {
                                    let reason = router_error_reason(&error);
                                    tracing::warn!(connection = %pump.generation, frame_bytes, reason, "Codex RPC frame rejected by router");
                                    break reason;
                                }
                                // Mirror the stdio pump: one socket read can buffer
                                // hundreds of messages. Let the event consumer run
                                // between them rather than route them all first.
                                tokio::task::yield_now().await;
                            }
                            Some(Ok(Message::Text(text))) => {
                                tracing::warn!(connection = %pump.generation, frame_bytes = text.len(), limit_bytes = pump.read_limit,
                                    "Codex WebSocket size limit exceeded");
                                break "websocket_size_limit";
                            }
                            Some(Ok(Message::Ping(_))) => {
                                // tungstenite queues the matching pong while reading.
                                let result = tokio::select! {
                                    _ = pump.cancel.cancelled() => break "connection_cancelled",
                                    result = tokio::time::timeout(REQUEST_TIMEOUT, socket.flush()) => result,
                                };
                                match result {
                                    Ok(Ok(())) => {},
                                    Ok(Err(error)) => break socket_error_reason(&pump, "pong_flush", &error),
                                    Err(_) => break "websocket_pong_deadline",
                                }
                            }
                            Some(Ok(Message::Pong(_))) => {}
                            Some(Ok(Message::Close(close))) => {
                                let close_code = close.as_ref().map(|frame| u16::from(frame.code));
                                tracing::warn!(connection = %pump.generation, ?close_code, "Codex WebSocket peer closed");
                                break "websocket_peer_close";
                            }
                            Some(Ok(_)) => break "websocket_unexpected_frame",
                            Some(Err(error)) => break socket_error_reason(&pump, "receive", &error),
                            None => break "websocket_eof",
                        }
                    }
                }
            };
            pump.stop_reason(reason);
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
                shared_identity: None,
                history: Arc::default(),
                history_admission: Arc::new(tokio::sync::Semaphore::new(1)),
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

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            locked(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn diagnostic_peer() -> (Arc<CodexClient>, CodexEvents, WebSocketStream<UnixStream>) {
        use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
        let (left, right) = UnixStream::pair().unwrap();
        let config = WebSocketConfig::default()
            .max_frame_size(Some(MAX_SHARED_READ_FRAME_BYTES))
            .max_message_size(Some(MAX_SHARED_READ_FRAME_BYTES));
        let socket = WebSocketStream::from_raw_socket(left, Role::Client, Some(config)).await;
        let server = WebSocketStream::from_raw_socket(right, Role::Server, None).await;
        let (client, events) = CodexClient::from_websocket(
            socket,
            "/synthetic/home".into(),
            "/synthetic/socket".into(),
        );
        // Isolated framing diagnostics, not an actual daemon/auth handshake.
        client.state.ready.store(true, Ordering::Release);
        (client, events, server)
    }

    #[tokio::test]
    async fn shared_disconnect_diagnostics_classify_frames_and_redact_payloads() {
        use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
        let _interest_guard = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let _capture = tracing::subscriber::set_default(subscriber);
        const SECRET: &str = "PRIVATE_MESSAGE_BASE64_TOKEN_CLOSE_TEXT";
        for (message, expected) in [
            (
                Message::Text(format!("not-json-{SECRET}").into()),
                "websocket_invalid_json",
            ),
            (
                Message::Text(
                    json!({"id":1,"error":{"code":"invalid","message":SECRET}})
                        .to_string()
                        .into(),
                ),
                "rpc_router_invalid_response_envelope",
            ),
            (
                Message::Text("x".repeat(MAX_FRAME_BYTES + 1).into()),
                "websocket_size_limit",
            ),
            (
                Message::Binary(SECRET.as_bytes().to_vec().into()),
                "websocket_unexpected_frame",
            ),
            (
                Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: SECRET.into(),
                })),
                "websocket_peer_close",
            ),
        ] {
            let (client, _events, mut server) = diagnostic_peer().await;
            // A peer can observe the close while writing an over-limit frame.
            let _ = server.send(message).await;
            tokio::time::timeout(Duration::from_secs(2), client.cancel_token().cancelled())
                .await
                .unwrap();
            assert_eq!(client.state.disconnect_reason.get(), Some(&expected));
            assert!(!client.is_ready());
            assert!(client
                .call("thread/list", json!({"limit":1}))
                .await
                .unwrap_err()
                .to_string()
                .contains("no longer ready"));
            client.stop();
            assert_eq!(
                client.state.disconnect_reason.get(),
                Some(&expected),
                "cleanup must not replace first cause"
            );
        }
        let log = String::from_utf8(locked(&capture.0).clone()).unwrap();
        for category in [
            "websocket_invalid_json",
            "rpc_router_invalid_response_envelope",
            "websocket_size_limit",
            "websocket_unexpected_frame",
            "websocket_peer_close",
        ] {
            assert!(
                log.contains(category),
                "missing diagnostic category {category}"
            );
        }
        assert!(log.contains("frame_bytes="));
        assert!(log.contains("limit_bytes=4194304"));
        assert!(log.contains("close_code=Some(1000)"));
        assert!(!log.contains(SECRET));
    }

    #[tokio::test]
    async fn shared_disconnect_diagnostics_keep_read_cancellation_and_mutation_fences() {
        let (client, _events, mut server) = diagnostic_peer().await;
        let pending = tokio::spawn({
            let client = client.clone();
            async move { client.call("thread/list", json!({"limit":1})).await }
        });
        assert!(server.next().await.unwrap().unwrap().is_text());
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(client.is_ready());
        assert!(client.state.disconnect_reason.get().is_none());
        assert_eq!(locked(&client.state.pending).len(), 1);
        server
            .send(Message::Text(
                json!({"id":1,"result":{"data":[],"nextCursor":null}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !locked(&client.state.pending).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(client.is_ready());
        let pending = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .call("thread/resume", json!({"threadId":"synthetic"}))
                    .await
            }
        });
        assert!(server.next().await.unwrap().unwrap().is_text());
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(
            client.state.disconnect_reason.get(),
            Some(&"mutation_caller_cancelled")
        );
        assert!(!client.is_ready());
    }

    #[tokio::test]
    async fn shared_disconnect_diagnostics_distinguish_deadline_from_socket_errors() {
        let (mut client, _events, mut server) = diagnostic_peer().await;
        Arc::get_mut(&mut client).unwrap().timeout = Duration::from_millis(30);
        let (result, frame) = tokio::join!(
            client.call("thread/resume", json!({"threadId":"synthetic"})),
            server.next()
        );
        assert!(frame.unwrap().unwrap().is_text());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("outcome unknown; not retried"));
        assert_eq!(
            client.state.disconnect_reason.get(),
            Some(&"mutation_response_deadline")
        );
        let (mut client, _events, mut server) = diagnostic_peer().await;
        Arc::get_mut(&mut client).unwrap().timeout = Duration::from_millis(30);
        let (result, frame) = tokio::join!(
            client.call("thread/list", json!({"limit":1})),
            server.next()
        );
        assert!(frame.unwrap().unwrap().is_text());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("read deadline exceeded; not retried"));
        assert!(client.is_ready());
        assert!(client.state.disconnect_reason.get().is_none());
        let (client, _events, _server) = diagnostic_peer().await;
        let error = tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "PRIVATE_IO_ERROR",
        ));
        assert_eq!(
            socket_error_reason(&client.state, "send", &error),
            "websocket_io_error"
        );
        assert_eq!(diagnostic_method("PRIVATE_UNKNOWN_METHOD"), "other");
        client.stop();
    }

    #[tokio::test]
    async fn shared_disconnect_diagnostics_distinguish_backlog_from_consumer_close() {
        let (client, _events, _server) = diagnostic_peer().await;
        for _ in 0..QUEUE_SIZE {
            client.state.emit(CodexEvent::Disconnected).unwrap();
        }
        assert!(client.state.emit(CodexEvent::Disconnected).is_err());
        assert_eq!(
            client.state.disconnect_reason.get(),
            Some(&"event_queue_overflow")
        );
        let (client, events, _server) = diagnostic_peer().await;
        drop(events);
        assert!(client.state.emit(CodexEvent::Disconnected).is_err());
        assert_eq!(
            client.state.disconnect_reason.get(),
            Some(&"event_consumer_closed")
        );
    }

    /// A fast turn streams roughly one `item/agentMessage/delta` per token,
    /// and the daemon's socket buffer delivers them as one burst. A consumer
    /// that is momentarily parked (mirror lock, scheduling) must not turn
    /// that burst into a lost connection, and no text may be lost or moved.
    #[tokio::test]
    async fn shared_delta_burst_behind_a_stalled_consumer_keeps_the_connection() {
        let (client, mut events, mut server) = diagnostic_peer().await;
        locked(&client.state.subscribed).insert("synthetic-thread".into(), 0);
        let mut expected = String::new();
        for n in 0..1_200 {
            let delta = format!("word{n} ");
            expected.push_str(&delta);
            let frame = json!({"method":"item/agentMessage/delta","params":{"threadId":"synthetic-thread","turnId":"synthetic-turn","itemId":"synthetic-message","delta":delta}});
            server
                .feed(Message::Text(frame.to_string().into()))
                .await
                .unwrap();
        }
        server.flush().await.unwrap();
        // The reply follows the whole burst on the wire, so it is routed only
        // after every delta has been offered to the (unread) event queue.
        let call = tokio::spawn({
            let client = client.clone();
            async move { client.call("model/list", json!({})).await }
        });
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match server.next().await {
                    Some(Ok(Message::Text(text))) => {
                        break serde_json::from_str::<Value>(&text).unwrap()
                    }
                    Some(Ok(_)) => continue,
                    other => panic!("server lost the client: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        server
            .send(Message::Text(
                json!({"id":request["id"],"result":{"data":[]}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reply.is_ok(),
            "burst closed the connection: {:?}",
            client.state.disconnect_reason.get()
        );
        assert!(client.is_ready());
        let mut text = String::new();
        let mut queued = 0;
        while let Ok(event) = events.try_recv() {
            queued += 1;
            let CodexEvent::Notification { method, params } = event else {
                panic!("unexpected event");
            };
            assert_eq!(method, "item/agentMessage/delta");
            assert_eq!(params["itemId"], "synthetic-message");
            text.push_str(params["delta"].as_str().unwrap());
        }
        assert_eq!(text, expected, "deltas lost, duplicated or reordered");
        assert!(
            queued < QUEUE_SIZE,
            "a run of one item's deltas holds {queued} slots"
        );
        client.stop();
    }

    pub(crate) struct Fixture {
        pub(crate) client: Arc<CodexClient>,
        pub(crate) events: CodexEvents,
        requests: Arc<Mutex<Vec<Value>>>,
        pub(crate) send: mpsc::Sender<Value>,
        disconnected: Arc<AtomicBool>,
        connections: Arc<AtomicU64>,
        task: tokio::task::JoinHandle<()>,
        _home: TestDirectory,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.client.stop();
            self.task.abort();
        }
    }
    async fn stream_image_reply(
        server: &mut WebSocketStream<UnixStream>,
        id: &Value,
        bytes: usize,
        image: &str,
        next: &Value,
        fragmented: bool,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        use tokio::io::AsyncWriteExt;
        use tokio_tungstenite::tungstenite::protocol::frame::{
            coding::{Data, OpCode},
            Frame,
        };
        let envelope = json!({"id":id,"result":{"data":[{"turnId":"large-turn","item":{
            "id":image,"type":"imageGeneration","status":"completed","savedPath":format!("/synthetic/{image}.png"),
            "result":"SYNTHETIC_PAYLOAD"
        }}],"nextCursor":next}}).to_string();
        let (prefix, suffix) = envelope.split_once("SYNTHETIC_PAYLOAD").unwrap();
        let block = vec![b'A'; 1024 * 1024];
        if fragmented {
            server
                .send(Message::Frame(Frame::message(
                    prefix.as_bytes().to_vec(),
                    OpCode::Data(Data::Text),
                    false,
                )))
                .await?;
            let mut left = bytes;
            while left > 0 {
                let take = left.min(block.len());
                server
                    .send(Message::Frame(Frame::message(
                        block[..take].to_vec(),
                        OpCode::Data(Data::Continue),
                        false,
                    )))
                    .await?;
                left -= take;
            }
            server
                .send(Message::Frame(Frame::message(
                    suffix.as_bytes().to_vec(),
                    OpCode::Data(Data::Continue),
                    true,
                )))
                .await?;
        } else {
            server.flush().await?;
            let wire = (prefix.len() + bytes + suffix.len()) as u64;
            server.get_mut().write_all(&[0x81, 127]).await?;
            server.get_mut().write_all(&wire.to_be_bytes()).await?;
            server.get_mut().write_all(prefix.as_bytes()).await?;
            let mut left = bytes;
            while left > 0 {
                let take = left.min(block.len());
                server.get_mut().write_all(&block[..take]).await?;
                left -= take;
            }
            server.get_mut().write_all(suffix.as_bytes()).await?;
            server.get_mut().flush().await?;
        }
        Ok(())
    }
    impl Fixture {
        pub(crate) async fn new(handler: impl Fn(&Value) -> Value + Send + 'static) -> Self {
            Self::with_deferred(handler, None).await
        }

        async fn with_deferred(
            handler: impl Fn(&Value) -> Value + Send + 'static,
            deferred: Option<(&'static str, Arc<tokio::sync::Notify>)>,
        ) -> Self {
            Self::with_outcomes(move |request| Ok(handler(request)), deferred).await
        }

        async fn with_outcomes(
            handler: impl Fn(&Value) -> Result<Value, Value> + Send + 'static,
            deferred: Option<(&'static str, Arc<tokio::sync::Notify>)>,
        ) -> Self {
            Self::with_fragmentation(handler, deferred, false).await
        }

        async fn with_fragmentation(
            handler: impl Fn(&Value) -> Result<Value, Value> + Send + 'static,
            deferred: Option<(&'static str, Arc<tokio::sync::Notify>)>,
            fragmented: bool,
        ) -> Self {
            use sha2::{Digest, Sha256};
            use std::os::unix::{
                ffi::OsStrExt,
                fs::{symlink, PermissionsExt},
            };
            let home = TestDirectory::new();
            let control = home.0.join("app-server-control");
            let protected = home.0.join("p");
            std::fs::create_dir(&control).unwrap();
            std::fs::create_dir(&protected).unwrap();
            std::fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o700)).unwrap();
            let alias = control.join("app-server-control.sock");
            let socket_path =
                protected.join(hex::encode(Sha256::digest(alias.as_os_str().as_bytes())));
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            symlink(&socket_path, &alias).unwrap();
            let home_path = home.0.clone();
            let requests = Arc::new(Mutex::new(vec![]));
            let seen = requests.clone();
            let disconnected = Arc::new(AtomicBool::new(false));
            let closed = disconnected.clone();
            let connections = Arc::new(AtomicU64::new(0));
            let accepted_count = connections.clone();
            let (send, outbound) = mpsc::channel::<Value>(32);
            let handler = Arc::new(Mutex::new(handler));
            let task = tokio::spawn(async move {
                let mut children = tokio::task::JoinSet::new();
                let mut first = Some(outbound);
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            accepted_count.fetch_add(1, Ordering::SeqCst);
                            let mut outbound = first.take();
                            let seen = seen.clone(); let home_path = home_path.clone();
                            let deferred = deferred.clone(); let handler = handler.clone(); let closed = closed.clone();
                            children.spawn(async move {
                            let primary = outbound.is_some();
                            let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
                            loop { tokio::select! {
                        message = async { match &mut outbound { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                            let Some(message) = message else { break; };
                            if server.send(Message::Text(message.to_string().into())).await.is_err() { break; }
                        }
                        message = server.next() => {
                            let Some(Ok(Message::Text(text))) = message else { break; };
                            let request: Value = serde_json::from_str(&text).unwrap();
                            locked(&seen).push(request.clone());
                            let Some(id) = request.get("id") else { continue; };
                            if let Some((method, gate)) = &deferred {
                                if request["method"] == *method {
                                    gate.notified().await;
                                }
                            }
                             let result = match request["method"].as_str() {
                                Some("initialize") => Ok(json!({"userAgent":"codex-tui/0.162.1", "codexHome":home_path})),
                                Some("account/read") => Ok(json!({"requiresOpenaiAuth":false,"account":null})),
                                 _ => locked(&handler)(&request),
                             };
                             if let Ok(result) = &result {
                                 if let Some(size) = result["syntheticFrameLength"].as_u64() {
                                     use tokio::io::AsyncWriteExt;
                                     if server.get_mut().write_all(&[0x81,127]).await.is_err() {break;}
                                     if server.get_mut().write_all(&size.to_be_bytes()).await.is_err() {break;}
                                     continue;
                                 }
                                 if let Some(size) = result["syntheticImageBytes"].as_u64() {
                                     if stream_image_reply(&mut server,id,size as usize,
                                         result["image"].as_str().unwrap(),&result["nextCursor"],fragmented).await.is_err() {break;}
                                     continue;
                                 }
                             }
                            let response = match result {
                                Ok(result) => json!({"id":id,"result":result}),
                                Err(error) => json!({"id":id,"error":error}),
                            };
                            let text = response.to_string();
                            if fragmented && text.len() > MAX_SHARED_READ_FRAME_BYTES {
                                use tokio_tungstenite::tungstenite::protocol::frame::{Frame, coding::{Data, OpCode}};
                                let chunks: Vec<_> = text.as_bytes().chunks(8 * 1024 * 1024).collect();
                                for (index, chunk) in chunks.iter().enumerate() {
                                    let opcode = if index == 0 { OpCode::Data(Data::Text) } else { OpCode::Data(Data::Continue) };
                                    if server.send(Message::Frame(Frame::message(chunk.to_vec(), opcode, index + 1 == chunks.len()))).await.is_err() { break; }
                                }
                            } else if server.send(Message::Text(text.into())).await.is_err() { break; }
                        }
                            }}
                            if primary { closed.store(true, Ordering::Release); }
                            });
                        }
                        _ = children.join_next(), if !children.is_empty() => {}
                    }
                }
            });
            let (client, events) = super::super::super::daemon::connect_verified(
                home.0.clone(),
                socket_path,
                None,
                None,
            )
            .await
            .unwrap();
            Self {
                client,
                events,
                requests,
                send,
                disconnected,
                connections,
                task,
                _home: home,
            }
        }
    }

    fn native_registry(
        fixture: &Fixture,
    ) -> Arc<super::super::super::native_sessions::NativeSessions> {
        use crate::agents::adapters::codex::native_sessions::{NativeSessions, MIGRATION};
        NativeSessions::new(
            crate::platform::store::sqlite::SqliteStore::open(
                &fixture._home.0.join("native.sqlite"),
                None,
                &[MIGRATION],
            )
            .unwrap(),
        )
    }

    fn recovering_driver(
        fixture: &Fixture,
        registry: Arc<super::super::super::native_sessions::NativeSessions>,
    ) -> CodexDriver {
        let mut driver = CodexDriver::new(fixture.client.clone());
        driver.restore_native_sessions(Some(registry));
        driver.observe_terminals(Some(crate::agents::terminal_runtime::TerminalRuntime::new(
            &[],
        )));
        driver
    }

    #[tokio::test]
    async fn shared_image_history_page_over_four_mib_keeps_connection_and_projection() {
        const ID: &str = "00000000-0000-4000-8000-000000000031";
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" => json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}}),
            "thread/items/list" => json!({"data":(0..3).map(|n| json!({
                "turnId":"synthetic-turn", "item":{"type":"imageGeneration","id":format!("image-{n}"),
                "status":"completed","revisedPrompt":"synthetic image","savedPath":"/synthetic/cat.png",
                "result":"A".repeat(2_670_000)}
            })).collect::<Vec<_>>(),"nextCursor":null}),
            "thread/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("unexpected RPC {other}"),
        }).await;
        let driver = CodexDriver::new(fixture.client.clone());
        let timeline = driver.get_timeline(ID, 100).await.unwrap();
        assert_eq!(
            timeline.len(),
            6,
            "three tools and three saved-file previews"
        );
        let projected = serde_json::to_string(&timeline).unwrap();
        assert!(projected.len() < 16 * 1024);
        assert!(
            !projected.contains(&"A".repeat(256)),
            "base64 must not leak into projected tool metadata"
        );
        assert!(fixture.client.is_ready());
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| matches!(r["method"].as_str(), Some("thread/resume" | "turn/start"))));
    }

    #[tokio::test]
    async fn isolated_history_oversize_preserves_primary_live_and_committed_cursor() {
        use crate::agents::domain::AgentSessionId;
        use crate::agents::manager::AgentManager;
        use crate::agents::ports::mirror::SessionMirrorPort;
        const ID: &str = "00000000-0000-4000-8000-000000000041";
        // One oversized item, an aggregate of individually valid items, and
        // fragmented aggregate overflow all exercise the real WS decoder.
        for fragmented in [false, true] {
            let reads = Arc::new(AtomicU64::new(0));
            let seen_reads = reads.clone();
            let mut fixture = Fixture::with_fragmentation(move |request| Ok(match request["method"].as_str().unwrap() {
                "thread/read" | "thread/start" => json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}}),
                "thread/items/list" => match seen_reads.fetch_add(1, Ordering::SeqCst) {
                    0 => json!({"data":(0..100).map(|n|json!({"turnId":"turn","item":{"id":format!("old-{n}"),"type":"agentMessage","text":"synthetic retained answer"}})).collect::<Vec<_>>(),"nextCursor":"older"}),
                    1 | 2 => json!({"syntheticFrameLength":MAX_HISTORY_FRAME_BYTES+1}),
                    3 => { assert_eq!(request["params"]["cursor"], "older", "failed page must not advance committed cursor"); json!({"data":[{"turnId":"turn","item":{"id":"older","type":"agentMessage","text":"synthetic older answer"}}],"nextCursor":null}) },
                    _ => panic!("no replay after the bounded capacity operation"),
                },
                "thread/list" => json!({"data":[],"nextCursor":null}),
                other => panic!("unexpected {other}"),
            }), None, fragmented).await;
            let (tx, _) = tokio::sync::broadcast::channel(32);
            let manager = AgentManager::connect_codex(
                fixture.client.clone(),
                std::mem::replace(&mut fixture.events, events::channel(1).1),
                tx,
            );
            let asid = AgentSessionId(ID.into());
            let before = manager
                .sessions()
                .get_snapshot(&asid)
                .await
                .unwrap()
                .timeline;
            assert!(!before.is_empty());
            let error = manager
                .sessions()
                .refresh_snapshot(&asid)
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("history read failed; primary connection retained"));
            assert_eq!(
                manager.mirror().get_snapshot(&asid).await.unwrap().timeline,
                before
            );
            assert!(fixture.client.is_ready());
            fixture
                .client
                .call("thread/list", json!({"limit":1}))
                .await
                .unwrap();
            assert_eq!(reads.load(Ordering::SeqCst), 3);
            // Cooldown declines a new request, without replaying the failed page.
            assert!(fixture
                .client
                .call("thread/items/list", json!({"threadId":ID,"limit":1}))
                .await
                .is_err());
            assert_eq!(reads.load(Ordering::SeqCst), 3);
            fixture
                .client
                .call("thread/start", json!({}))
                .await
                .unwrap();
            fixture.send.send(json!({"method":"thread/status/changed","params":{"threadId":ID,"status":{"type":"active","activeFlags":[]}}})).await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if manager
                        .mirror()
                        .session_info(&asid)
                        .await
                        .is_some_and(|info| {
                            info.status == crate::agents::domain::AgentSessionStatus::Busy
                        })
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert!(!manager
                .sessions()
                .load_earlier_timeline(&asid)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(reads.load(Ordering::SeqCst), 4);
            manager.shutdown();
        }
    }

    #[tokio::test]
    async fn adaptive_image_history_recovers_forty_mb_with_other_live_sessions_and_older_cursor() {
        const B: &str = "00000000-0000-4000-8000-000000000052";
        // Sixteen individually bounded image results reproduce the observed
        // ~41.6-MB aggregate, in ordinary and fragmented WebSocket messages.
        for fragmented in [false, true] {
            let mut fixture = Fixture::with_fragmentation(|request| Ok(match request["method"].as_str().unwrap() {
                "thread/start" => json!({"thread":{"id":"live-A","status":{"type":"idle"}}}),
                "turn/start" => json!({"turn":{"id":"turn-A","status":"inProgress"}}),
                "thread/read" => json!({"thread":{"id":request["params"]["threadId"],"historyMode":"paginated","status":{"type":"notLoaded"}}}),
                "thread/items/list" => {
                    let params = &request["params"];
                    assert_eq!(params["threadId"], B);
                    assert_eq!(params["sortDirection"], "desc");
                    if params["cursor"] == "older" {
                        json!({"data":[{"turnId":"older-turn","item":{"id":"older-text","type":"agentMessage","text":"retained older answer"}}],"nextCursor":null})
                    } else {
                        let start = params["cursor"].as_str().map(|s| s.parse::<usize>().unwrap()).unwrap_or(0);
                        let end = (start + params["limit"].as_u64().unwrap() as usize).min(16);
                        json!({"data":(start..end).map(|n|json!({"turnId":"image-turn","item":{
                            "type":"imageGeneration","id":format!("image-{n}"),"status":"completed",
                            "savedPath":format!("/synthetic/image-{n}.png"),"result":"A".repeat(2_600_000)
                        }})).collect::<Vec<_>>(),"nextCursor":if end==16 {"older".into()} else {end.to_string()}})
                    }
                },
                other => panic!("unexpected {other}"),
            }), None, fragmented).await;
            let driver = CodexDriver::new(fixture.client.clone());
            fixture
                .client
                .call("thread/start", json!({}))
                .await
                .unwrap();
            let history = driver.get_timeline(B, 32);
            let controls = async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !locked(&fixture.requests)
                        .iter()
                        .any(|r| r["method"] == "thread/items/list")
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                fixture
                    .client
                    .call("turn/start", json!({"threadId":"live-A","input":[]}))
                    .await
                    .unwrap();
                let c = fixture
                    .client
                    .call(
                        "thread/read",
                        json!({"threadId":"normal-C","includeTurns":false}),
                    )
                    .await
                    .unwrap();
                assert_eq!(c["thread"]["id"], "normal-C");
                fixture.send.send(json!({"method":"thread/status/changed","params":{"threadId":"live-A","status":{"type":"active","activeFlags":[]}}})).await.unwrap();
            };
            let (timeline, ()) = tokio::join!(history, controls);
            let timeline = timeline.unwrap();
            assert_eq!(
                timeline.len(),
                32,
                "all sixteen tools and original-file previews"
            );
            let tools: std::collections::HashSet<_> = timeline
                .iter()
                .filter_map(|row| match &row.part {
                    crate::agents::domain::AgentPart::Tool(tool) => Some(tool.id.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(tools.len(), 16, "no duplicated or skipped image items");
            assert!(!serde_json::to_string(&timeline)
                .unwrap()
                .contains(&"A".repeat(256)));
            assert!(fixture.client.is_ready());
            {
                let requests = locked(&fixture.requests);
                let pages: Vec<_> = requests
                    .iter()
                    .filter(|r| r["method"] == "thread/items/list")
                    .collect();
                assert_eq!(pages.len(), 2);
                assert_eq!(pages[0]["params"]["limit"], 8);
                assert_eq!(pages[1]["params"]["limit"], 8);
                assert_eq!(pages[1]["params"]["cursor"], "8");
                assert_eq!(
                    requests
                        .iter()
                        .filter(|r| r["method"] == "initialize")
                        .count(),
                    2,
                    "primary and one reader; small pages avoid a giant aggregate"
                );
            }
            let older = driver.load_earlier_timeline(B, 100).await.unwrap();
            assert_eq!(older.len(), 1);
            assert!(driver
                .load_earlier_timeline(B, 100)
                .await
                .unwrap()
                .is_empty());
            let mut events = std::mem::replace(&mut fixture.events, events::channel(1).1);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(CodexEvent::Notification { method, params }) = events.recv().await {
                        if method == "thread/status/changed" && params["threadId"] == "live-A" {
                            break;
                        }
                    }
                }
            })
            .await
            .unwrap();
            assert!(fixture.client.is_ready());
        }
    }

    #[tokio::test]
    async fn adaptive_history_caller_cancellation_stops_capacity_retries_and_total_deadline_closes_only_reader(
    ) {
        for capacity in [true, false] {
            let gate = Arc::new(tokio::sync::Notify::new());
            let fixture = Fixture::with_deferred(
                move |request| match request["method"].as_str().unwrap() {
                    "thread/items/list" => {
                        json!({"syntheticFrameLength":MAX_HISTORY_FRAME_BYTES+1})
                    }
                    "thread/list" => json!({"data":[],"nextCursor":null}),
                    other => panic!("unexpected {other}"),
                },
                Some(("thread/items/list", gate.clone())),
            )
            .await;
            let pending = tokio::spawn({
                let client = fixture.client.clone();
                async move {
                    client
                        .call_sized_budgeted(
                            "thread/items/list",
                            json!({"threadId":"qa","limit":100}),
                            tokio::time::Instant::now()
                                + Duration::from_millis(if capacity { 5000 } else { 100 }),
                            MAX_HISTORY_WIRE_WORK_BYTES,
                        )
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(2), async {
                while !locked(&fixture.requests)
                    .iter()
                    .any(|r| r["method"] == "thread/items/list")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if capacity {
                pending.abort();
                let _ = pending.await;
                assert_eq!(fixture.client.history_admission.available_permits(), 0);
                gate.notify_one();
            } else {
                assert!(pending.await.unwrap().is_err());
                gate.notify_one();
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while fixture.client.history_admission.available_permits() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                locked(&fixture.requests)
                    .iter()
                    .filter(|r| r["method"] == "thread/items/list")
                    .count(),
                1,
                "neither abandoned capacity failure nor total deadline starts a retry"
            );
            assert!(fixture.client.history.lock().await.client.is_none());
            assert!(fixture.client.is_ready());
            fixture
                .client
                .call("thread/list", json!({"limit":1}))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn adaptive_history_accounts_failed_wire_work_and_never_retries_other_errors() {
        for allowance in [MAX_HISTORY_WIRE_WORK_BYTES, 2 * MAX_SHARED_READ_FRAME_BYTES] {
            let fixture = Fixture::new(|request| {
                assert_eq!(request["method"], "thread/items/list");
                let count = request["params"]["limit"].as_u64().unwrap().min(16);
                if count > 1 { return json!({"syntheticFrameLength":MAX_HISTORY_FRAME_BYTES+1}); }
                json!({"data":(0..count).map(|n|json!({"turnId":"turn","item":{
                    "id":format!("image-{n}"),"type":"imageGeneration","result":"A".repeat(2_600_000)
                }})).collect::<Vec<_>>(),"nextCursor":"unchanged-next"})
            }).await;
            let result = fixture.client.call_sized_budgeted("thread/items/list",
                json!({"threadId":"qa","limit":100,"cursor":"opaque-start","turnId":"turn","sortDirection":"asc"}),
                tokio::time::Instant::now()+Duration::from_secs(20), allowance).await;
            let requests = locked(&fixture.requests);
            let pages: Vec<_> = requests
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .collect();
            if allowance == MAX_HISTORY_WIRE_WORK_BYTES {
                let (page, charged) = result.unwrap();
                assert_eq!(page["data"].as_array().unwrap().len(), 1);
                assert!(
                    charged > 2 * MAX_HISTORY_FRAME_BYTES + 2_600_000,
                    "failed decoder work must not disappear from sized-call accounting"
                );
                assert!(charged < allowance);
                assert_eq!(pages.len(), 2);
                for field in ["threadId", "cursor", "turnId", "sortDirection"] {
                    assert_eq!(pages[0]["params"][field], pages[1]["params"][field]);
                }
            } else {
                assert!(result.is_err());
                assert_eq!(
                    pages.len(),
                    1,
                    "exhausted failed-wire allowance must not admit a recovery"
                );
            }
            assert!(fixture.client.is_ready());
        }
        let fixture = Fixture::with_outcomes(
            |request| {
                assert_eq!(request["method"], "thread/items/list");
                Err(json!({"code":-32603,"message":"PRIVATE_UPSTREAM_ERROR"}))
            },
            None,
        )
        .await;
        assert!(fixture
            .client
            .call("thread/items/list", json!({"threadId":"qa","limit":100}))
            .await
            .is_err());
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            1
        );
        assert!(fixture.client.is_ready());
    }

    #[tokio::test]
    async fn adaptive_one_item_pages_preserve_selected_active_turn_recovery_beyond_sixteen_items() {
        const ID: &str = "00000000-0000-4000-8000-000000000053";
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" | "thread/resume" => json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"active","activeFlags":[]}}}),
            "thread/turns/list" => json!({"data":[{"id":"active-turn","status":"inProgress"}],"nextCursor":null}),
            "thread/items/list" => {
                assert_eq!(request["params"]["turnId"], "active-turn");
                assert_eq!(request["params"]["limit"], 1);
                let start = request["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
                let end = (start+1).min(17);
                json!({"data":(start..end).map(|n|json!({"turnId":"active-turn","item":{
                    "id":format!("item-{n}"),"type":"agentMessage","text":"synthetic retained answer"
                }})).collect::<Vec<_>>(),"nextCursor":if end==17 {None} else {Some(end.to_string())}})
            },
            other => panic!("unexpected {other}"),
        }).await;
        // Same state learned after capacity in the forty-MB integration test.
        fixture.client.history.lock().await.item_limit = Some(1);
        let driver = CodexDriver::new(fixture.client.clone());
        driver.prepare_selected_session(ID).await.unwrap();
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            17
        );
        assert!(fixture.client.is_ready());
    }

    #[tokio::test]
    async fn rollout_projection_capacity_exhausts_two_mib_without_a_recovery_connection() {
        use crate::agents::adapters::codex::rollout_projection::{canonical_items, ItemTarget};
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/items/list" => {
                let count = request["params"]["limit"].as_u64().unwrap();
                json!({"data":(0..count).map(|n|json!({"turnId":"turn","item":{
                    "id":format!("image-{n}"),"type":"imageGeneration","result":"A".repeat(5*1024*1024)
                }})).collect::<Vec<_>>(),"nextCursor":null})
            },
            "thread/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("unexpected {other}"),
        }).await;
        let driver = CodexDriver::new(fixture.client.clone());
        let targets = [ItemTarget {
            turn: "turn".into(),
            item: "image-0".into(),
        }];
        assert!(canonical_items(
            &driver,
            "qa",
            &targets,
            tokio::time::Instant::now() + Duration::from_secs(5)
        )
        .await
        .is_err());
        assert_eq!(fixture.connections.load(Ordering::SeqCst), 2,
            "primary plus first reader only: 64-MiB failed work already exceeds the 2-MiB rollout allowance");
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            2
        );
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            1
        );
        assert!(fixture.client.is_ready());
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rollout_projection_remaining_bytes_and_expired_deadline_do_not_dispatch_more_reads() {
        use crate::agents::adapters::codex::rollout_projection::{canonical_items, ItemTarget};
        let fixture = Fixture::new(|request| {
            assert_eq!(request["method"], "thread/items/list");
            assert_eq!(request["params"]["turnId"], "first-turn");
            let mut result = json!({"data":[{"turnId":"first-turn","item":{
                "id":"first","type":"agentMessage","text":"synthetic answer"
            }}],"nextCursor":null,"padding":""});
            let overhead = serde_json::to_vec(&json!({"id":request["id"],"result":result}))
                .unwrap()
                .len();
            result["padding"] = json!("A".repeat(2 * 1024 * 1024 - overhead));
            result
        })
        .await;
        let driver = CodexDriver::new(fixture.client.clone());
        let targets = [
            ItemTarget {
                turn: "first-turn".into(),
                item: "first".into(),
            },
            ItemTarget {
                turn: "second-turn".into(),
                item: "second".into(),
            },
        ];
        assert!(canonical_items(
            &driver,
            "qa",
            &targets,
            tokio::time::Instant::now() + Duration::from_secs(5)
        )
        .await
        .is_err());
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            1,
            "the first exact 2-MiB reply leaves no allowance for the second turn"
        );
        assert!(canonical_items(
            &driver,
            "qa",
            &targets,
            tokio::time::Instant::now() - Duration::from_millis(1)
        )
        .await
        .is_err());
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            1
        );
        assert_eq!(fixture.connections.load(Ordering::SeqCst), 2);
        assert!(fixture.client.is_ready());

        let empty = Fixture::new(|_| panic!("expired/empty work must not dispatch")).await;
        for (deadline, allowance) in [
            (
                tokio::time::Instant::now() - Duration::from_millis(1),
                MAX_HISTORY_WIRE_WORK_BYTES,
            ),
            (tokio::time::Instant::now() + Duration::from_secs(5), 0),
        ] {
            assert!(empty
                .client
                .call_sized_budgeted(
                    "thread/items/list",
                    json!({"threadId":"qa","limit":8}),
                    deadline,
                    allowance
                )
                .await
                .is_err());
        }
        assert_eq!(
            empty.connections.load(Ordering::SeqCst),
            1,
            "no history connection at all"
        );
        assert_eq!(empty.client.history_admission.available_permits(), 1);
        assert!(empty.client.is_ready());
    }

    #[tokio::test]
    async fn rollout_projection_inherits_unspent_five_second_deadline_and_releases_late_reader() {
        use crate::agents::adapters::codex::rollout_projection::{canonical_items, ItemTarget};
        let gate = Arc::new(tokio::sync::Notify::new());
        let fixture = Fixture::with_deferred(
            |request| match request["method"].as_str().unwrap() {
                "thread/items/list" | "thread/list" => json!({"data":[],"nextCursor":null}),
                other => panic!("unexpected {other}"),
            },
            Some(("thread/items/list", gate.clone())),
        )
        .await;
        let driver = CodexDriver::new(fixture.client.clone());
        // The enclosing poll has already spent four of its five seconds on
        // terminal/metadata/tail work. Projection must not start a new clock.
        let poll_started = tokio::time::Instant::now() - Duration::from_secs(4);
        let deadline = poll_started + Duration::from_secs(5);
        let targets = [ItemTarget {
            turn: "turn".into(),
            item: "item".into(),
        }];
        assert!(canonical_items(&driver, "qa", &targets, deadline)
            .await
            .is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.client.history_admission.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("inherited actor deadline releases the read; not a fresh 20-second deadline");
        assert!(fixture.client.history.lock().await.client.is_none());
        assert_eq!(fixture.connections.load(Ordering::SeqCst), 2);
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            1
        );
        gate.notify_one();
        assert!(fixture.client.is_ready());
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn bounded_history_window_publishes_complete_items_and_preserves_real_older_cursor() {
        const ID: &str = "00000000-0000-4000-8000-000000000061";
        let reads = Arc::new(AtomicU64::new(0));
        let seen = reads.clone();
        let fixture = Fixture::new(move |request| match request["method"].as_str().unwrap() {
            "thread/read" => json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}}),
            "thread/items/list" => {
                if seen.fetch_add(1,Ordering::SeqCst)==0 {
                    return json!({"syntheticFrameLength":MAX_HISTORY_FRAME_BYTES+1});
                }
                let cursor=request["params"]["cursor"].as_str().map(|s|s.parse::<usize>().unwrap()).unwrap_or(0);
                assert_eq!(request["params"]["limit"],1);
                json!({"data":[{"turnId":"turn","item":{"id":format!("image-{cursor}"),"type":"imageGeneration",
                    "status":"completed","savedPath":format!("/synthetic/{cursor}.png"),"result":"A".repeat(1024)}}],
                    "nextCursor":if cursor==2 {None} else {Some((cursor+1).to_string())}})
            },
            other=>panic!("unexpected {other}"),
        }).await;
        let driver = CodexDriver::new(fixture.client.clone());
        let mut rows = driver.get_timeline(ID, 100).await.unwrap();
        assert_eq!(
            rows.len(),
            2,
            "failed-work budget returns one complete item, not an empty window"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "no extra read after the bounded window is full"
        );
        rows.extend(driver.load_earlier_timeline(ID, 100).await.unwrap());
        assert_eq!(rows.len(), 6);
        assert!(driver
            .load_earlier_timeline(ID, 100)
            .await
            .unwrap()
            .is_empty());
        let ids: std::collections::HashSet<_> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids.len(), 6, "no duplicate or skipped projected rows");
        assert_eq!(reads.load(Ordering::SeqCst), 4);
        assert!(fixture.client.is_ready());
    }

    #[tokio::test]
    #[ignore = "explicit serial resource fixture; run under RLIMIT_AS/time/timeout, never a real daemon"]
    async fn resource_large_history_item_is_complete_and_continuable() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        const ID: &str = "00000000-0000-4000-8000-000000000062";
        let mib = std::env::var("MUQUN_SYNTHETIC_IMAGE_MIB")
            .unwrap_or_else(|_| "64".into())
            .parse::<usize>()
            .unwrap();
        assert!((1..=200).contains(&mib));
        let bytes = mib * 1024 * 1024;
        let fragmented = std::env::var("MUQUN_SYNTHETIC_FRAGMENTED").as_deref() == Ok("1");
        let mut fixture=Fixture::with_fragmentation(move |request|Ok(match request["method"].as_str().unwrap(){
            "thread/start"=>json!({"thread":{"id":"live-A","status":{"type":"idle"}}}),
            "turn/start"=>json!({"turn":{"id":"turn-A","status":"inProgress"}}),
            "thread/read"=>json!({"thread":{"id":request["params"]["threadId"],"historyMode":"paginated","status":{"type":"notLoaded"}}}),
            "thread/items/list"=>if request["params"]["cursor"]=="older" {
                json!({"data":[{"turnId":"old-turn","item":{"id":"old","type":"agentMessage","text":"complete older answer"}}],"nextCursor":null})
            }else {json!({"syntheticImageBytes":bytes,"image":"large-image","nextCursor":"older"})},
            "thread/list"=>json!({"data":[],"nextCursor":null}),
            other=>panic!("unexpected {other}"),
        }),None,fragmented).await;
        fixture
            .client
            .call("thread/start", json!({}))
            .await
            .unwrap();
        let history = fixture.client.call_sized(
            "thread/items/list",
            json!({"threadId":ID,"limit":100,"cursor":null}),
        );
        let controls = async {
            while !locked(&fixture.requests)
                .iter()
                .any(|r| r["method"] == "thread/items/list")
            {
                tokio::task::yield_now().await;
            }
            fixture
                .client
                .call("turn/start", json!({"threadId":"live-A","input":[]}))
                .await
                .unwrap();
            assert_eq!(
                fixture
                    .client
                    .call(
                        "thread/read",
                        json!({"threadId":"normal-C","includeTurns":false})
                    )
                    .await
                    .unwrap()["thread"]["id"],
                "normal-C"
            );
            fixture.send.send(json!({"method":"thread/status/changed","params":{"threadId":"live-A","status":{"type":"active","activeFlags":[]}}})).await.unwrap();
        };
        let (read, ()) = tokio::join!(history, controls);
        let (page, wire) = read.unwrap();
        let raw = page["data"][0]["item"]["result"].as_str().unwrap();
        assert_eq!(raw.len(), bytes);
        assert!(wire > bytes && wire < bytes + 4096);
        assert_eq!(page["nextCursor"], "older");
        let mut expected_raw = Sha256::new();
        let mut decoded = Sha256::new();
        let mut expected_decoded = Sha256::new();
        let mut decoded_bytes = 0usize;
        for chunk in raw.as_bytes().chunks(1024 * 1024) {
            expected_raw.update(vec![b'A'; chunk.len()]);
            let data = base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .unwrap();
            decoded_bytes += data.len();
            decoded.update(&data);
            expected_decoded.update(vec![0u8; data.len()]);
        }
        let raw_digest = Sha256::digest(raw.as_bytes());
        assert_eq!(raw_digest, expected_raw.finalize());
        let decoded_digest = decoded.finalize();
        assert_eq!(decoded_digest, expected_decoded.finalize());
        assert_eq!(decoded_bytes, bytes / 4 * 3);
        eprintln!("synthetic_resource wire_bytes={wire} base64_bytes={bytes} decoded_bytes={decoded_bytes} raw_sha256={} decoded_sha256={} fragmented={fragmented}",hex::encode(raw_digest),hex::encode(decoded_digest));
        drop(page);
        let driver = CodexDriver::new(fixture.client.clone());
        let tail = driver.get_timeline(ID, 1).await.unwrap();
        assert_eq!(
            tail.len(),
            1,
            "strict row limit without retaining the huge raw item"
        );
        let count = locked(&fixture.requests)
            .iter()
            .filter(|r| r["method"] == "thread/items/list")
            .count();
        assert_eq!(driver.load_earlier_timeline(ID, 1).await.unwrap().len(), 1);
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            count,
            "mapped residual survives without another image read"
        );
        assert_eq!(
            driver.load_earlier_timeline(ID, 100).await.unwrap().len(),
            1
        );
        assert!(driver
            .load_earlier_timeline(ID, 100)
            .await
            .unwrap()
            .is_empty());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(CodexEvent::Notification { method, params }) =
                    fixture.events.recv().await
                {
                    if method == "thread/status/changed" && params["threadId"] == "live-A" {
                        break;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert!(fixture.client.is_ready());
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
        assert_eq!(fixture.client.state.read_limit, MAX_SHARED_READ_FRAME_BYTES);
        if mib <= 64 {
            let gate = Arc::new(tokio::sync::Notify::new());
            let late=Fixture::with_deferred(move |_|json!({"syntheticImageBytes":bytes,"image":"late-image","nextCursor":null}),
                Some(("thread/items/list",gate.clone()))).await;
            let pending = tokio::spawn({
                let client = late.client.clone();
                async move {
                    client
                        .call("thread/items/list", json!({"threadId":ID,"limit":1}))
                        .await
                }
            });
            while !locked(&late.requests)
                .iter()
                .any(|r| r["method"] == "thread/items/list")
            {
                tokio::task::yield_now().await;
            }
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            assert_eq!(late.client.history_admission.available_permits(), 0);
            gate.notify_one();
            tokio::time::timeout(Duration::from_secs(20), async {
                while late.client.history_admission.available_permits() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                locked(&late.requests)
                    .iter()
                    .filter(|r| r["method"] == "thread/items/list")
                    .count(),
                1
            );
            assert!(late.client.is_ready());
            eprintln!("synthetic_resource late_cancel_bytes={bytes} admission_released=true primary_ready=true");
        }
    }

    #[tokio::test]
    async fn isolated_history_cancel_reserves_slot_until_late_reply_and_never_mutates() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let fixture = Fixture::with_deferred(
            |request| match request["method"].as_str().unwrap() {
                "thread/items/list" | "thread/list" => json!({"data":[],"nextCursor":null}),
                "thread/turns/list" => {
                    assert_eq!(request["params"]["itemsView"], "full");
                    json!({"data":[],"nextCursor":null})
                }
                "thread/read" => {
                    assert_eq!(request["params"]["includeTurns"], true);
                    json!({"thread":{"turns":[]}})
                }
                other => panic!("unexpected {other}"),
            },
            Some(("thread/items/list", gate.clone())),
        )
        .await;
        let pending = tokio::spawn({
            let client = fixture.client.clone();
            async move {
                client
                    .call(
                        "thread/items/list",
                        json!({"threadId":"synthetic","limit":1}),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !locked(&fixture.requests)
                .iter()
                .any(|r| r["method"] == "thread/items/list")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        let _ = pending.await;
        assert!(fixture
            .client
            .call(
                "thread/items/list",
                json!({"threadId":"synthetic","limit":1})
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("already pending"));
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.client.history_admission.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let history = fixture.client.history.lock().await.client.clone().unwrap();
        let primary_next = fixture.client.state.next_id.load(Ordering::Acquire);
        fixture
            .client
            .call(
                "thread/turns/list",
                json!({"threadId":"synthetic","itemsView":"full","limit":1}),
            )
            .await
            .unwrap();
        fixture
            .client
            .call(
                "thread/read",
                json!({"threadId":"synthetic","includeTurns":true}),
            )
            .await
            .unwrap();
        assert_eq!(
            fixture.client.state.next_id.load(Ordering::Acquire),
            primary_next,
            "legacy/full-turn image reads must not use primary RPCs"
        );
        let count = locked(&fixture.requests).len();
        for method in [
            "thread/resume",
            "thread/start",
            "thread/fork",
            "turn/start",
            "turn/interrupt",
        ] {
            assert!(matches!(
                history.call(method, json!({})).await,
                Err(crate::agents::ports::agent::AgentError::Unsupported(_))
            ));
        }
        assert!(history
            .write(json!({"id":77,"result":{"decision":"accept"}}))
            .await
            .is_err());
        assert_eq!(locked(&fixture.requests).len(), count);
        fixture.client.stop();
        assert!(!history.is_ready());
        assert!(history.cancel_token().is_cancelled());
    }

    #[tokio::test]
    async fn isolated_history_refuses_replaced_socket_without_replay_or_primary_loss() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new(|request| {
            assert_eq!(request["method"], "thread/list");
            json!({"data":[],"nextCursor":null})
        })
        .await;
        let alias = fixture
            ._home
            .0
            .join("app-server-control/app-server-control.sock");
        let physical = std::fs::read_link(alias).unwrap();
        std::fs::remove_file(&physical).unwrap();
        let _replacement = tokio::net::UnixListener::bind(&physical).unwrap();
        std::fs::set_permissions(&physical, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(fixture
            .client
            .call(
                "thread/items/list",
                json!({"threadId":"synthetic","limit":1})
            )
            .await
            .is_err());
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
        assert!(fixture.client.is_ready());
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "initialize")
                .count(),
            1,
            "no replacement history socket may be initialized"
        );
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "thread/items/list"));
    }

    #[tokio::test]
    async fn shared_prompt_delivery_does_not_relock_mutations_and_keeps_turn_serialization() {
        use crate::agents::ports::agent::AgentError;

        const ID: &str = "00000000-0000-4000-8000-000000000021";
        let turns = Arc::new(AtomicU64::new(0));
        let accepted = turns.clone();
        let fixture = Fixture::new(move |request| match request["method"].as_str().unwrap() {
            "thread/start" | "thread/read" => {
                json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"idle"}}})
            }
            "turn/start" => {
                let n = accepted.fetch_add(1, Ordering::Relaxed) + 1;
                json!({"turn":{"id":format!("turn-{n}"),"status":"inProgress"}})
            }
            "turn/steer" => json!({"turnId":request["params"]["expectedTurnId"]}),
            other => panic!("prompt delivery must not call {other}"),
        })
        .await;
        let driver = CodexDriver::new(fixture.client.clone());
        driver.create_session(None, None, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            driver
                .send_prompt(ID, "First prompt", &[], None)
                .await
                .unwrap();
            driver
                .send_prompt(ID, "Steer the current turn", &[], Some("steer"))
                .await
                .unwrap();
            assert!(matches!(
                driver
                    .send_prompt(ID, "Not a new turn while busy", &[], None)
                    .await,
                Err(AgentError::InvalidRequest(_))
            ));
            fixture
                .send
                .send(json!({"method":"turn/completed","params":{
                    "threadId":ID,"turn":{"id":"turn-1","status":"completed"}
                }}))
                .await
                .unwrap();
            while fixture.client.active_turn(ID).is_some() {
                tokio::task::yield_now().await;
            }
            let (left, right) = tokio::join!(
                driver.send_prompt(ID, "Follow-up A", &[], None),
                driver.send_prompt(ID, "Follow-up B", &[], None),
            );
            assert!(matches!(
                (&left, &right),
                (Ok(()), Err(AgentError::InvalidRequest(_)))
                    | (Err(AgentError::InvalidRequest(_)), Ok(()))
            ));
        })
        .await
        .expect("prompt delivery must finish without acquiring its mutation lock twice");
        assert_eq!(turns.load(Ordering::Relaxed), 2);
        let requests = locked(&fixture.requests);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "turn/start")
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "turn/steer")
                .count(),
            1
        );
        assert!(!requests.iter().any(|r| r["method"] == "thread/resume"));
    }

    #[tokio::test]
    async fn native_recovery_restores_only_selected_recorded_session_without_replaying_a_turn() {
        const ID: &str = "00000000-0000-4000-8000-000000000011";
        let fixture = Fixture::new(|request| {
            let thread = |status| json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":status}}});
            match request["method"].as_str().unwrap() {
                "thread/start" | "thread/resume" => thread("idle"),
                "thread/read" => thread("notLoaded"),
                "thread/items/list" | "thread/turns/list" => json!({"data":[],"nextCursor":null}),
                "turn/start" => {
                    assert_eq!(request["params"]["input"][0]["text"], "Only this new prompt");
                    json!({"turn":{"id":"new-turn","status":"completed"}})
                }
                "thread/delete" => json!({}),
                other => panic!("unexpected {other}"),
            }
        }).await;
        let registry = native_registry(&fixture);
        let driver = recovering_driver(&fixture, registry.clone());
        driver.create_session(None, None, None).await.unwrap();
        assert!(registry.contains(&fixture._home.0, ID).await.unwrap());
        fixture
            .send
            .send(json!({"method":"thread/closed","params":{"threadId":ID}}))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.client.is_subscribed(ID) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(driver);
        let restored = recovering_driver(&fixture, registry.clone());
        restored.get_session(ID).await.unwrap();
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "thread/resume"));
        restored.get_timeline(ID, 10).await.unwrap();
        restored.get_timeline(ID, 10).await.unwrap();
        assert_eq!(
            restored.runtime(ID, true).await.unwrap().source,
            RuntimeSource::Native
        );
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
            .all(|r| r["params"] == json!({"threadId":ID,"excludeTurns":true})));
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "turn/start"));
        restored
            .send_prompt(ID, "Only this new prompt", &[], None)
            .await
            .unwrap();
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "turn/start")
                .count(),
            1
        );
        restored.delete_session(ID).await.unwrap();
        assert!(!registry.contains(&fixture._home.0, ID).await.unwrap());
    }

    #[tokio::test]
    async fn native_recovery_does_not_resume_unknown_or_other_home_records() {
        const ID: &str = "00000000-0000-4000-8000-000000000012";
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}})
            }
            "thread/items/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("unowned history must not call {other}"),
        })
        .await;
        let registry = native_registry(&fixture);
        let driver = recovering_driver(&fixture, registry.clone());
        driver.get_timeline(ID, 10).await.unwrap();
        registry
            .record(&fixture._home.0.join("other-home"), ID)
            .await
            .unwrap();
        driver.get_timeline(ID, 10).await.unwrap();
        assert!(!fixture.client.is_subscribed(ID));
        assert!(!registry.contains(&fixture._home.0, ID).await.unwrap());
    }

    #[tokio::test]
    async fn native_recovery_provenance_never_overrides_an_official_writer_refusal() {
        const ID: &str = "00000000-0000-4000-8000-000000000013";
        let fixture = Fixture::with_outcomes(|request| match request["method"].as_str().unwrap() {
            "thread/read" => Ok(json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}})),
            "thread/resume" => Err(json!({"code":-32600,"message":"thread already has an active writer"})),
            other => panic!("refused resume must not call {other}"),
        }, None).await;
        let registry = native_registry(&fixture);
        registry.record(&fixture._home.0, ID).await.unwrap();
        let driver = recovering_driver(&fixture, registry);
        assert!(driver.get_timeline(ID, 10).await.is_err());
        assert!(!fixture.client.is_subscribed(ID));
        assert_ne!(
            driver.runtime(ID, true).await.unwrap().source,
            RuntimeSource::Native
        );
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/resume")
                .count(),
            1
        );
        assert!(driver
            .send_prompt(ID, "Not submitted", &[], None)
            .await
            .is_err());
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "turn/start" || r["method"] == "thread/start"));
    }

    #[tokio::test]
    async fn selected_native_recovery_runs_after_mark_viewed_seeds_the_mirror() {
        use crate::agents::domain::AgentSessionId;
        use crate::agents::manager::AgentManager;
        const ID: &str = "00000000-0000-4000-8000-000000000018";
        for mode in ["cached", "cold", "refresh"] {
            let mut fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}})
            }
            "thread/resume" => {
                json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"idle"}}})
            }
            "thread/turns/list" => json!({"data":[],"nextCursor":null}),
            "thread/items/list" => json!({"data":[
                {"turnId":"initial","item":{"type":"agentMessage","id":"answer","text":"Original answer"}},
                {"turnId":"initial","item":{"type":"commandExecution","id":"shell","command":"printf QA_C4_INITIAL","cwd":"/synthetic/project","status":"completed","exitCode":0,"aggregatedOutput":"QA_C4_INITIAL"}},
                {"turnId":"initial","item":{"type":"userMessage","id":"question","content":[{"type":"text","text":"Original question"}]}}
            ],"nextCursor":null}),
            other => panic!("selected recovery must not call {other}"),
        })
        .await;
            let registry = native_registry(&fixture);
            registry.record(&fixture._home.0, ID).await.unwrap();
            let herdr = crate::test_support::FakeHerdr::start(vec![], None);
            herdr.set_agents(json!([{"pane_id":"w1:p1","terminal_id":"fixture-terminal","agent":"codex","agent_status":"idle","shell_pid":42,"agent_session":{"kind":"id","agent":"codex","value":ID}}]));
            let (tx, _events) = tokio::sync::broadcast::channel(64);
            let manager = AgentManager::connect_codex_recoverable(
                fixture.client.clone(),
                std::mem::replace(&mut fixture.events, events::channel(1).1),
                tx,
                Some(crate::agents::terminal_runtime::TerminalRuntime::new(&[
                    herdr.session(),
                ])),
                Some(registry),
            );
            if mode != "cold" {
                manager.view_session(ID, 1).await.unwrap();
            }
            assert_eq!(
                manager
                    .sessions()
                    .get_session(ID)
                    .await
                    .unwrap()
                    .runtime
                    .unwrap()
                    .source,
                RuntimeSource::Terminal
            );
            assert!(!fixture.client.is_subscribed(ID));
            let asid = AgentSessionId(ID.into());
            let first = if mode == "refresh" {
                manager.sessions().refresh_snapshot(&asid).await.unwrap()
            } else {
                manager.sessions().get_snapshot(&asid).await.unwrap()
            };
            assert_eq!(
                first.info.runtime.as_ref().unwrap().source,
                RuntimeSource::Native
            );
            assert_eq!(first.info.agent_id, "codex");
            assert_eq!(first.timeline.len(), 3);
            let wire = serde_json::to_value(&first).unwrap();
            assert_eq!(wire["info"]["agent_id"], "codex");
            assert_eq!(wire["timeline"].as_array().unwrap().len(), 3);
            assert!(first
                .timeline
                .iter()
                .any(|item| item.role == crate::agents::domain::TimelineRole::User));
            assert!(first.timeline.iter().any(|item| matches!(&item.part, crate::agents::domain::AgentPart::Tool(call) if call.status == crate::agents::domain::ToolCallStatus::Completed)));
            let second = manager.sessions().get_snapshot(&asid).await.unwrap();
            assert_eq!(second.info.agent_id, "codex");
            assert_eq!(second.timeline, first.timeline);
            assert_eq!(
                locked(&fixture.requests)
                    .iter()
                    .filter(|r| r["method"] == "thread/items/list")
                    .count(),
                1
            );
            assert_eq!(second.info.runtime.unwrap().source, RuntimeSource::Native);
            assert_eq!(
                locked(&fixture.requests)
                    .iter()
                    .filter(|r| r["method"] == "thread/resume")
                    .count(),
                1
            );
            assert!(!locked(&fixture.requests)
                .iter()
                .any(|r| r["method"] == "turn/start"));
            manager.shutdown();
        }
    }

    #[tokio::test]
    async fn selected_empty_history_is_read_once_even_for_concurrent_snapshots() {
        use crate::agents::domain::AgentSessionId;
        use crate::agents::manager::AgentManager;
        const ID: &str = "00000000-0000-4000-8000-000000000020";
        let mut fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}})
            }
            "thread/items/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("unknown empty history must not call {other}"),
        })
        .await;
        let (tx, _events) = tokio::sync::broadcast::channel(64);
        let manager = AgentManager::connect_codex(
            fixture.client.clone(),
            std::mem::replace(&mut fixture.events, events::channel(1).1),
            tx,
        );
        manager.view_session(ID, 1).await.unwrap();
        let asid = AgentSessionId(ID.into());
        let (first, second) = tokio::join!(
            manager.sessions().get_snapshot(&asid),
            manager.sessions().get_snapshot(&asid)
        );
        for snapshot in [
            first.unwrap(),
            second.unwrap(),
            manager.sessions().get_snapshot(&asid).await.unwrap(),
        ] {
            assert_eq!(snapshot.info.agent_id, "codex");
            assert!(snapshot.timeline.is_empty());
        }
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count(),
            1
        );
        assert!(!fixture.client.is_subscribed(ID));
        manager.shutdown();
    }

    #[tokio::test]
    async fn selected_native_recovery_refusal_preserves_cached_history_and_never_sends() {
        use crate::agents::domain::AgentSessionId;
        use crate::agents::manager::AgentManager;
        use crate::agents::ports::mirror::SessionMirrorPort;
        const ID: &str = "00000000-0000-4000-8000-000000000019";
        let mut fixture = Fixture::with_outcomes(|request| match request["method"].as_str().unwrap() {
            "thread/read" => Ok(json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"notLoaded"}}})),
            "thread/resume" => Err(json!({"code":-32600,"message":"thread already has an active writer"})),
            other => panic!("refused selected recovery must not call {other}"),
        }, None).await;
        let registry = native_registry(&fixture);
        registry.record(&fixture._home.0, ID).await.unwrap();
        let (tx, _events) = tokio::sync::broadcast::channel(64);
        let manager = AgentManager::connect_codex_recoverable(
            fixture.client.clone(),
            std::mem::replace(&mut fixture.events, events::channel(1).1),
            tx,
            Some(crate::agents::terminal_runtime::TerminalRuntime::new(&[])),
            Some(registry),
        );
        manager.view_session(ID, 1).await.unwrap();
        let asid = AgentSessionId(ID.into());
        let retained = crate::agents::adapters::codex::mapper::item(
            ID,
            "old-turn",
            &json!({"type":"agentMessage","id":"old-message","text":"Retained history"}),
            0,
        );
        assert!(!retained.is_empty());
        manager
            .mirror()
            .upsert_timeline_items(&asid, retained)
            .await;
        let before = manager.mirror().get_snapshot(&asid).await.unwrap().timeline;
        assert!(manager.sessions().get_snapshot(&asid).await.is_err());
        assert_eq!(
            manager.mirror().get_snapshot(&asid).await.unwrap().timeline,
            before
        );
        assert!(!fixture.client.is_subscribed(ID));
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/resume")
                .count(),
            1
        );
        assert!(manager
            .agent()
            .send_prompt(ID, "Do not submit", &[], None)
            .await
            .is_err());
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/resume")
                .count(),
            2
        );
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "turn/start" || r["method"] == "thread/start"));
        manager.shutdown();
    }

    #[tokio::test]
    async fn native_recovery_records_fork_and_explicit_send_but_not_loaded_rejoin() {
        const ID: &str = "00000000-0000-4000-8000-000000000016";
        const CHILD: &str = "00000000-0000-4000-8000-000000000017";
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/read" | "thread/resume" => {
                json!({"thread":{"id":ID,"historyMode":"paginated","status":{"type":"idle"}}})
            }
            "thread/items/list" | "thread/turns/list" => json!({"data":[],"nextCursor":null}),
            "thread/fork" => json!({"thread":{"id":CHILD,"status":{"type":"idle"}}}),
            "turn/start" => json!({"turn":{"id":"accepted-turn","status":"completed"}}),
            other => panic!("unexpected {other}"),
        })
        .await;
        let registry = native_registry(&fixture);
        let driver = recovering_driver(&fixture, registry.clone());
        driver.get_timeline(ID, 10).await.unwrap();
        assert!(fixture.client.is_subscribed(ID));
        assert!(!registry.contains(&fixture._home.0, ID).await.unwrap());
        driver.fork_session(ID, None).await.unwrap();
        assert!(registry.contains(&fixture._home.0, CHILD).await.unwrap());
        assert!(!registry.contains(&fixture._home.0, ID).await.unwrap());
        driver
            .send_prompt(ID, "Explicit new prompt", &[], None)
            .await
            .unwrap();
        assert!(registry.contains(&fixture._home.0, ID).await.unwrap());
    }

    #[tokio::test]
    async fn native_recovery_record_failure_does_not_reject_an_accepted_create_fork_or_send() {
        use crate::agents::adapters::codex::native_sessions::{NativeSessions, MIGRATION};
        const ID: &str = "00000000-0000-4000-8000-000000000014";
        const CHILD: &str = "00000000-0000-4000-8000-000000000015";
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/start" | "thread/read" => json!({"thread":{"id":ID,"status":{"type":"idle"}}}),
            "thread/fork" => json!({"thread":{"id":CHILD,"status":{"type":"idle"}}}),
            "turn/start" => json!({"turn":{"id":"accepted-turn","status":"completed"}}),
            other => panic!("unexpected {other}"),
        })
        .await;
        let db = crate::platform::store::sqlite::SqliteStore::open(
            &fixture._home.0.join("unwritable.sqlite"),
            None,
            &[MIGRATION],
        )
        .unwrap();
        db.call(|db| {
            db.execute_batch("PRAGMA query_only=ON")?;
            Ok(())
        })
        .await
        .unwrap();
        let registry = NativeSessions::new(db);
        let driver = recovering_driver(&fixture, registry.clone());
        driver.create_session(None, None, None).await.unwrap();
        driver.fork_session(ID, None).await.unwrap();
        driver
            .send_prompt(ID, "Accepted once", &[], None)
            .await
            .unwrap();
        assert!(!registry.contains(&fixture._home.0, ID).await.unwrap());
        assert!(!registry.contains(&fixture._home.0, CHILD).await.unwrap());
        for method in ["thread/start", "thread/fork", "turn/start"] {
            assert_eq!(
                locked(&fixture.requests)
                    .iter()
                    .filter(|r| r["method"] == method)
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn cancelled_catalog_read_does_not_poison_native_creation_on_same_connection() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let fixture = Fixture::with_deferred(
            |request| match request["method"].as_str().unwrap() {
                "config/read" => json!({"config":{"model_provider":"openai"}}),
                "thread/start" => json!({"thread":{"id":"new-thread","status":{"type":"idle"}}}),
                method => panic!("unexpected method {method}"),
            },
            Some(("config/read", gate.clone())),
        )
        .await;
        let client = fixture.client.clone();
        let read = tokio::spawn(async move {
            client
                .call("config/read", json!({"includeLayers":false}))
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !locked(&fixture.requests)
                .iter()
                .any(|r| r["method"] == "config/read")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Discovery/session lookup deadlines drop the read future, not the
        // service connection or its owned threads. The daemon may reply later.
        read.abort();
        assert!(read.await.unwrap_err().is_cancelled());
        assert!(
            fixture.client.is_ready(),
            "cancelled read disconnected native manager"
        );
        gate.notify_one();
        let created = fixture
            .client
            .call("thread/start", json!({}))
            .await
            .unwrap();
        assert_eq!(created["thread"]["id"], "new-thread");
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|r| r["method"] == "thread/start")
                .count(),
            1
        );
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
            std::mem::replace(&mut fixture.events, events::channel(1).1),
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
            if request["method"] == "thread/name/set" {
                assert_eq!(request["params"]["threadId"], "00000000-0000-4000-8000-000000000001");
                assert_eq!(request["params"]["name"], "Synthetic task title");
                return json!({});
            }
            assert_eq!(request["method"], "thread/start");
            assert_eq!(request["params"]["developerInstructions"], "Synthetic owner instructions. Synthetic task completion rule.");
            assert_eq!(request["params"]["config"]["mcp_servers.muqun.http_headers_helper"], "fixture-helper --task-id synthetic-task");
            assert_eq!(request["params"]["cwd"], "/synthetic/project");
            json!({"thread":{"id":"00000000-0000-4000-8000-000000000001","historyMode":"paginated","status":{"type":"idle"}}})
        }).await;
        let registry = native_registry(&fixture);
        let driver = recovering_driver(&fixture, registry.clone());
        let arguments = ["-c", "projects={ '/synthetic/project' = { trust_level = 'trusted' } }", "-c", "developer_instructions='Synthetic owner instructions. Synthetic task completion rule.'", "-c", "mcp_servers.muqun.http_headers_helper='fixture-helper --task-id synthetic-task'"]
            .into_iter().map(str::to_owned).collect::<Vec<_>>();
        let prepared = driver
            .prepare_terminal_launch("/synthetic/project", "Synthetic task title", &arguments)
            .await
            .unwrap();
        assert_eq!(
            prepared.arguments,
            [
                "--remote".to_owned(),
                format!(
                    "unix://{}",
                    std::fs::read_link(
                        fixture
                            ._home
                            .0
                            .join("app-server-control/app-server-control.sock")
                    )
                    .unwrap()
                    .display()
                ),
                "resume".into(),
                prepared.session.asid.0.clone()
            ]
        );
        assert!(fixture.client.is_subscribed(&prepared.session.asid.0));
        assert!(!registry
            .contains(&fixture._home.0, &prepared.session.asid.0)
            .await
            .unwrap());
        assert_eq!(prepared.session.title, "Synthetic task title");
        let methods: Vec<String> = locked(&fixture.requests)
            .iter()
            .filter_map(|request| request["method"].as_str())
            .filter(|method| method.starts_with("thread/") || method.starts_with("turn/"))
            .map(str::to_owned)
            .collect();
        assert_eq!(methods, ["thread/start", "thread/name/set"]);
        assert!(!locked(&fixture.requests)
            .iter()
            .any(|r| r["method"] == "turn/start"));
        assert!(matches!(
            driver
                .prepare_terminal_launch(
                    "/synthetic/project",
                    "Synthetic task title",
                    &["--no-daemon".into()]
                )
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
    async fn shared_terminal_does_not_attach_before_persistence_ack_or_retry_a_disconnect() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let fixture = Fixture::with_deferred(
            |request| {
                assert_eq!(request["method"], "thread/start");
                json!({"thread":{"id":"00000000-0000-4000-8000-000000000001","status":{"type":"idle"}}})
            },
            Some(("thread/name/set", gate)),
        )
        .await;
        let driver = CodexDriver::new(fixture.client.clone());
        let arguments = vec![
            "-c".into(),
            "projects={ '/synthetic/project' = { trust_level = 'trusted' } }".into(),
        ];
        let prepared = driver.prepare_terminal_launch(
            "/synthetic/project",
            "Synthetic task title",
            &arguments,
        );
        tokio::pin!(prepared);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut prepared)
                .await
                .is_err()
        );
        assert!(locked(&fixture.requests)
            .iter()
            .any(|request| request["method"] == "thread/name/set"));
        fixture.client.stop();
        assert!(matches!(prepared.await, Err(AgentError::NotAvailable(_))));
        assert_eq!(
            locked(&fixture.requests)
                .iter()
                .filter(|request| request["method"] == "thread/start")
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
        for (cancelled, invalid_response, invalid_task, invalid_persistence) in [
            (false, false, false, false),
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            let task_id = if invalid_task {
                crate::tasks::domain::TaskId("invalid".into())
            } else {
                crate::tasks::domain::TaskId::new()
            };
            let expected_task_id = task_id.0.clone();
            let mut fixture = Fixture::new(move |request| {
                if request["method"] == "thread/name/set" {
                    assert_eq!(request["params"]["threadId"], "00000000-0000-4000-8000-000000000001");
                    assert_eq!(request["params"]["name"], "Named task");
                    return if invalid_persistence { Value::Null } else { json!({}) };
                }
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
                "id":task_id.0, "kind":"delegation", "goal":"synthetic goal", "label":"Named task", "cwd":cwd,
                "binding":{"kind":"pane","session_id":"default","pane_id":"w1:p1","tab_id":"w1:t1","agent_id":"codex"},
                "state":if cancelled {"cancelled"} else {"dispatching"},
                "created_by":{"kind":"assistant","id":"synthetic"}, "created_ms":1,"updated_ms":1,"seq":1,"origin":"mcp"
            })).unwrap();
            state.tasks.insert_for_test(&task).await;
            let (tx, _) = tokio::sync::broadcast::channel(64);
            let manager = AgentManager::connect_codex(
                fixture.client.clone(),
                std::mem::replace(&mut fixture.events, events::channel(1).1),
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
                    fleet: None,
                }),
            )
            .await;
            if let Some(started) = to_type {
                assert!(!cancelled && !invalid_response && !invalid_task && !invalid_persistence);
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
                usize::from(
                    !cancelled && !invalid_response && !invalid_task && !invalid_persistence
                )
            );
            if cancelled || invalid_response || invalid_task || invalid_persistence {
                assert_eq!(launch.prompt_delivery, PromptDelivery::NotAttempted);
                assert_eq!(launch.payload["prompt_submitted"], false);
            } else {
                assert_eq!(launch.prompt_delivery, PromptDelivery::Attempted);
                assert_eq!(
                    starts[0]["params"]["args"],
                    json!([
                        "--remote",
                        format!(
                            "unix://{}",
                            std::fs::read_link(
                                fixture
                                    ._home
                                    .0
                                    .join("app-server-control/app-server-control.sock")
                            )
                            .unwrap()
                            .display()
                        ),
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
                usize::from(
                    !cancelled && !invalid_response && !invalid_task && !invalid_persistence
                )
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
    async fn isolated_history_and_primary_shutdown_leave_shared_listener_alive_without_mutations() {
        let fixture = Fixture::new(|request| match request["method"].as_str().unwrap() {
            "thread/items/list" | "thread/list" => json!({"data":[],"nextCursor":null}),
            other => panic!("shutdown must not issue {other}"),
        })
        .await;
        fixture
            .client
            .call("thread/items/list", json!({"threadId":"qa","limit":1}))
            .await
            .unwrap();
        let history = fixture.client.history.lock().await.client.clone().unwrap();
        history.stop();
        assert!(fixture.client.is_ready());
        fixture
            .client
            .call("thread/list", json!({"limit":1}))
            .await
            .unwrap();
        fixture.client.stop();
        assert!(history.cancel_token().is_cancelled());
        let socket = std::fs::read_link(
            fixture
                ._home
                .0
                .join("app-server-control/app-server-control.sock"),
        )
        .unwrap();
        let (next, _events) = super::super::super::daemon::connect_verified(
            fixture._home.0.clone(),
            socket,
            None,
            None,
        )
        .await
        .unwrap();
        next.call("thread/list", json!({"limit":1})).await.unwrap();
        assert!(
            !fixture.task.is_finished(),
            "shared listener outlives both disconnected clients"
        );
        next.stop();
        assert!(locked(&fixture.requests).iter().all(|r| matches!(
            r["method"].as_str(),
            Some(
                "initialize" | "initialized" | "account/read" | "thread/items/list" | "thread/list"
            )
        )));
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
