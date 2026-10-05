use super::*;
use crate::agents::domain::{AgentSessionId, AgentSessionStatus};
use crate::*;

use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest as _};
use tower_http::compression::{predicate::Predicate as _, CompressionLayer};

const TOKEN: &str = "device-token";

fn status_event(asid: &str, seq: u64) -> AgentDomainEvent {
    AgentDomainEvent::StatusChanged {
        asid: AgentSessionId(asid.to_owned()),
        status: AgentSessionStatus::Busy,
        error: None,
        seq,
    }
}

// ---------------------------------------------------------------------------
// Frames, codec, subscriptions, registry
// ---------------------------------------------------------------------------

#[test]
fn client_frames_parse_and_anything_else_is_refused() {
    assert_eq!(
        ClientFrame::parse(r#"{"t":"subscribe","asid":"ses_1"}"#),
        Ok(ClientFrame::Subscribe("ses_1".into()))
    );
    assert_eq!(
        ClientFrame::parse(r#"{"t":"unsubscribe","asid":"ses_1","extra":1}"#),
        Ok(ClientFrame::Unsubscribe("ses_1".into()))
    );
    assert_eq!(
        ClientFrame::parse(r#"{"t":"subscribe_all"}"#),
        Ok(ClientFrame::SubscribeAll)
    );
    assert_eq!(ClientFrame::parse(r#"{"t":"ping"}"#), Ok(ClientFrame::Ping));

    assert_eq!(
        ClientFrame::parse(r#"{"t":"request","path":"/api"}"#),
        Err(FrameError::UnknownType)
    );
    assert_eq!(
        ClientFrame::parse("not json"),
        Err(FrameError::InvalidFrame)
    );
    assert_eq!(
        ClientFrame::parse(r#"{"asid":"x"}"#),
        Err(FrameError::InvalidFrame)
    );
    assert_eq!(
        ClientFrame::parse(r#"{"t":"subscribe"}"#),
        Err(FrameError::InvalidFrame)
    );
    assert_eq!(
        ClientFrame::parse(r#"{"t":"subscribe","asid":""}"#),
        Err(FrameError::InvalidAsid)
    );
    let long = format!(r#"{{"t":"subscribe","asid":"{}"}}"#, "a".repeat(129));
    assert_eq!(ClientFrame::parse(&long), Err(FrameError::InvalidAsid));
    assert_eq!(
        ClientFrame::parse(r#"{"t":"subscribe","asid":"a\nb"}"#),
        Err(FrameError::InvalidAsid)
    );
}

#[test]
fn server_frames_are_the_documented_strings() {
    assert_eq!(
        hello_frame("c-1"),
        r#"{"t":"hello","connection_id":"c-1","protocol":1}"#
    );
    assert_eq!(resync_frame("ses_1"), r#"{"t":"resync","asid":"ses_1"}"#);
    assert_eq!(
        subscribed_frame(Some("ses_1")),
        r#"{"t":"subscribed","asid":"ses_1"}"#
    );
    assert_eq!(subscribed_frame(None), r#"{"t":"subscribed","all":true}"#);
    assert_eq!(
        error_frame(FrameError::OutOfOrder),
        r#"{"t":"error","code":"out_of_order"}"#
    );
    assert_eq!(PONG_FRAME, r#"{"t":"pong"}"#);
}

/// `event` and `data` are the SSE record's own, byte for byte: `data` is
/// embedded as the JSON it is rather than parsed and re-serialized.
#[test]
fn an_event_frame_carries_the_sse_name_and_data_verbatim() {
    let event = status_event("ses_1", 7);
    let (name, data) = agent_event_record(&event);
    let frame = event_frame(&event);
    assert_eq!(
        frame,
        format!(r#"{{"t":"event","asid":"ses_1","seq":7,"event":"{name}","data":{data}}}"#)
    );
    let parsed: Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(parsed["event"], "agent.status.changed");
    assert_eq!(parsed["data"]["type"], "agent.status.changed");
}

fn sealed_pair() -> (FrameCodec, [u8; 32], [u8; 32]) {
    let material = [5_u8; 32];
    let codec = FrameCodec::Sealed(Box::new(
        SealedCodec::new(&material, "GET /api/ws", "nonce-1", "conn-1").unwrap(),
    ));
    let server = transport::derive_ws_server_key(&material, "conn-1", "nonce-1").unwrap();
    let client = transport::derive_ws_client_key(&material, "conn-1", "nonce-1").unwrap();
    (codec, server, client)
}

fn client_frame(client_key: &[u8; 32], seq: u64, plaintext: &str) -> String {
    let aad = format!("GET /api/ws\nconn-1\n{seq}");
    let c = transport::seal_stream_event(client_key, seq, aad.as_bytes(), plaintext.as_bytes())
        .unwrap();
    format!(r#"{{"seq":{seq},"c":"{c}"}}"#)
}

#[test]
fn sealed_frames_carry_seq_and_the_first_names_the_connection() {
    let (mut codec, server_key, _) = sealed_pair();
    let first: Value = serde_json::from_str(&codec.encode("{\"t\":\"a\"}").unwrap()).unwrap();
    let second: Value = serde_json::from_str(&codec.encode("{\"t\":\"b\"}").unwrap()).unwrap();
    assert_eq!(first["seq"], 0);
    assert_eq!(first["cid"], "conn-1");
    assert_eq!(second["seq"], 1);
    assert!(second.get("cid").is_none());
    let opened = transport::open_stream_event(
        &server_key,
        1,
        b"GET /api/ws\nconn-1\n1",
        second["c"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(opened, b"{\"t\":\"b\"}");

    let mut plain = FrameCodec::Plain;
    assert_eq!(plain.encode("{\"t\":\"a\"}").unwrap(), "{\"t\":\"a\"}");
}

#[test]
fn a_client_frame_must_be_the_next_seq_and_sealed_under_the_client_key() {
    let (mut codec, server_key, client_key) = sealed_pair();
    // A gap is refused before anything is decrypted.
    assert_eq!(
        codec.decode(&client_frame(&client_key, 1, r#"{"t":"ping"}"#)),
        Err(FrameError::OutOfOrder)
    );
    assert_eq!(
        codec
            .decode(&client_frame(&client_key, 0, r#"{"t":"ping"}"#))
            .unwrap(),
        r#"{"t":"ping"}"#
    );
    // A replay of seq 0 is now out of order too.
    assert_eq!(
        codec.decode(&client_frame(&client_key, 0, r#"{"t":"ping"}"#)),
        Err(FrameError::OutOfOrder)
    );
    // The right seq under the server's key -- a reflected frame -- fails.
    assert_eq!(
        codec.decode(&client_frame(&server_key, 1, r#"{"t":"ping"}"#)),
        Err(FrameError::InvalidFrame)
    );
    // A plaintext frame on a sealed connection is not a frame.
    assert_eq!(
        codec.decode(r#"{"t":"ping"}"#),
        Err(FrameError::InvalidFrame)
    );
}

#[test]
fn subscriptions_filter_bound_and_resync() {
    let mut subs = Subscriptions::default();
    assert!(!subs.wants("ses_1"));
    assert!(!subs.wants(""), "a socket watching nothing gets nothing");

    subs.apply(&ClientFrame::Subscribe("ses_1".into())).unwrap();
    assert!(subs.wants("ses_1"));
    assert!(!subs.wants("ses_2"));
    assert!(subs.wants(""), "a sessionless event reaches every watcher");
    assert_eq!(subs.resync_targets(), vec!["ses_1".to_string()]);

    subs.apply(&ClientFrame::Unsubscribe("ses_1".into()))
        .unwrap();
    assert!(!subs.wants("ses_1"));

    for i in 0..MAX_SUBSCRIPTIONS {
        subs.apply(&ClientFrame::Subscribe(format!("ses_{i}")))
            .unwrap();
    }
    assert_eq!(
        subs.apply(&ClientFrame::Subscribe("one_more".into())),
        Err(FrameError::TooManySubscriptions)
    );
    // Re-subscribing to one already held is not growth.
    assert!(subs.apply(&ClientFrame::Subscribe("ses_0".into())).is_ok());

    subs.apply(&ClientFrame::SubscribeAll).unwrap();
    assert!(subs.wants("anything"));
    assert_eq!(subs.resync_targets(), vec![String::new()]);
    assert_eq!(
        subs.apply(&ClientFrame::Ping).unwrap().as_deref(),
        Some(PONG_FRAME)
    );
}

#[test]
fn the_newest_socket_of_a_device_supersedes_its_oldest() {
    let registry = Arc::new(WsRegistry::default());
    let mut slots: Vec<WsSlot> = (0..MAX_CONNECTIONS_PER_DEVICE)
        .map(|_| registry.admit("phone-1").unwrap())
        .collect();
    let newest = registry.admit("phone-1").unwrap();
    assert_eq!(registry.open_count(), MAX_CONNECTIONS_PER_DEVICE);
    assert!(
        slots[0].evicted.try_recv().is_ok(),
        "the oldest socket is told to go"
    );
    assert!(slots[1].evicted.try_recv().is_err());
    drop(newest);
    assert_eq!(registry.open_count(), MAX_CONNECTIONS_PER_DEVICE - 1);
}

#[test]
fn the_gateway_wide_cap_refuses_rather_than_evicting_another_device() {
    let registry = Arc::new(WsRegistry::default());
    let _held: Vec<WsSlot> = (0..MAX_CONNECTIONS_TOTAL)
        .map(|i| {
            registry
                .admit(&format!("phone-{}", i / MAX_CONNECTIONS_PER_DEVICE))
                .unwrap()
        })
        .collect();
    assert!(registry.admit("another-phone").is_none());
    // A device already at its own cap still replaces its own oldest.
    assert!(registry.admit("phone-0").is_some());
}

// ---------------------------------------------------------------------------
// The route, over a real socket
// ---------------------------------------------------------------------------

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The route behind the same transport middleware the gateway runs.
async fn serve(state: AppState) -> std::net::SocketAddr {
    let app = super::mount(Router::new())
        .layer(
            CompressionLayer::new()
                .compress_when(DefaultPredicate::new().and(SizeAbove::new(COMPRESSION_MIN_BYTES))),
        )
        .layer(middleware::from_fn(envelope_compression_gate))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            encrypted_transport,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn connect(
    addr: std::net::SocketAddr,
    headers: &[(&'static str, String)],
) -> Result<(Client, tungstenite::handshake::client::Response), tungstenite::Error> {
    let mut request = format!("ws://{addr}{WS_PATH}")
        .into_client_request()
        .unwrap();
    // Asked for, so the compression layer has a reason to touch the 101 and
    // the test would see it if it did.
    request
        .headers_mut()
        .insert("accept-encoding", "gzip".parse().unwrap());
    for (name, value) in headers {
        request.headers_mut().insert(*name, value.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await
}

fn bearer() -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {TOKEN}"))]
}

/// The next text frame, skipping control frames. `None` when the socket
/// closed first.
async fn next_text(client: &mut Client) -> Option<String> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), client.next())
            .await
            .expect("the gateway said nothing for five seconds")?;
        match message.ok()? {
            tungstenite::Message::Text(text) => return Some(text.to_string()),
            tungstenite::Message::Close(_) => return None,
            _ => continue,
        }
    }
}

async fn next_json(client: &mut Client) -> Value {
    serde_json::from_str(&next_text(client).await.expect("socket closed")).unwrap()
}

async fn send_text(client: &mut Client, text: &str) {
    client
        .send(tungstenite::Message::Text(text.to_owned().into()))
        .await
        .unwrap();
}

/// Waits for the socket to end, however it ends.
async fn closes(client: &mut Client) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(message)) = client.next().await {
            if matches!(message, tungstenite::Message::Close(_)) {
                return;
            }
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn a_plaintext_device_gets_a_plaintext_hello() {
    let state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    let addr = serve(state).await;
    let (mut client, response) = connect(addr, &bearer()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert!(response.headers().get(TRANSPORT_HEADER).is_none());
    assert!(response.headers().get("content-encoding").is_none());
    let hello = next_json(&mut client).await;
    assert_eq!(hello["t"], "hello");
    assert_eq!(hello["protocol"], 1);
    assert!(uuid::Uuid::parse_str(hello["connection_id"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn an_unpaired_or_missing_token_never_upgrades() {
    let state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    let addr = serve(state).await;
    for (headers, status) in [
        (
            vec![("authorization", String::from("Bearer wrong-token"))],
            StatusCode::FORBIDDEN,
        ),
        (Vec::new(), StatusCode::UNAUTHORIZED),
    ] {
        match connect(addr, &headers).await {
            Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), status),
            other => panic!(
                "expected {status}, got {:?}",
                other.map(|(_, r)| r.status())
            ),
        }
    }
}

/// An encrypted device, and the headers of its sealed empty GET.
fn encrypted_device() -> (DeviceRecord, Vec<u8>) {
    let transport_key = generate_token();
    let material = transport::decode_key(&transport_key).unwrap();
    let mut device = test_device("phone-1", TOKEN);
    device.transport_key = Some(transport_key);
    (device, material)
}

fn sealed_upgrade_headers(material: &[u8]) -> (Vec<(&'static str, String)>, String) {
    let payload = EncryptedRequestPayload {
        token: TOKEN.into(),
        content_type: None,
        body: String::new(),
    };
    let envelope = transport::seal(
        material,
        transport::Direction::Request,
        b"GET /api/ws",
        &serde_json::to_vec(&payload).unwrap(),
        now_unix_ms(),
    )
    .unwrap();
    let nonce = envelope.nonce.clone();
    let headers = vec![
        (TRANSPORT_HEADER, String::from("1")),
        (TRANSPORT_DEVICE_HEADER, String::from("phone-1")),
        (
            TRANSPORT_ENVELOPE_HEADER,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&envelope).unwrap()),
        ),
    ];
    (headers, nonce)
}

/// The App's side of a sealed socket, written from the documented
/// derivation rather than from the server's codec.
struct AppCodec {
    server_key: [u8; 32],
    client_key: [u8; 32],
    connection_id: String,
    recv_seq: u64,
    send_seq: u64,
}

impl AppCodec {
    /// Reads the sealed hello: the connection id from the clear `cid`, then
    /// the key it names, then the frame.
    async fn open(client: &mut Client, material: &[u8], request_nonce: &str) -> Self {
        let first = next_json(client).await;
        assert_eq!(first["seq"], 0);
        let connection_id = first["cid"].as_str().unwrap().to_owned();
        let mut codec = Self {
            server_key: transport::derive_ws_server_key(material, &connection_id, request_nonce)
                .unwrap(),
            client_key: transport::derive_ws_client_key(material, &connection_id, request_nonce)
                .unwrap(),
            connection_id,
            recv_seq: 0,
            send_seq: 0,
        };
        let hello: Value = serde_json::from_str(&codec.open_frame(&first)).unwrap();
        assert_eq!(hello["t"], "hello");
        assert_eq!(hello["connection_id"], codec.connection_id.as_str());
        codec
    }

    fn open_frame(&mut self, frame: &Value) -> String {
        let seq = frame["seq"].as_u64().unwrap();
        assert_eq!(seq, self.recv_seq, "server frames arrive in order");
        let aad = format!("GET /api/ws\n{}\n{seq}", self.connection_id);
        let plaintext = transport::open_stream_event(
            &self.server_key,
            seq,
            aad.as_bytes(),
            frame["c"].as_str().unwrap(),
        )
        .unwrap();
        self.recv_seq += 1;
        String::from_utf8(plaintext).unwrap()
    }

    async fn recv(&mut self, client: &mut Client) -> Value {
        let frame = next_json(client).await;
        serde_json::from_str(&self.open_frame(&frame)).unwrap()
    }

    fn seal_at(&self, seq: u64, plaintext: &str) -> String {
        let aad = format!("GET /api/ws\n{}\n{seq}", self.connection_id);
        let c = transport::seal_stream_event(
            &self.client_key,
            seq,
            aad.as_bytes(),
            plaintext.as_bytes(),
        )
        .unwrap();
        format!(r#"{{"seq":{seq},"c":"{c}"}}"#)
    }

    async fn send(&mut self, client: &mut Client, plaintext: &str) {
        let frame = self.seal_at(self.send_seq, plaintext);
        self.send_seq += 1;
        send_text(client, &frame).await;
    }
}

#[tokio::test]
async fn a_sealed_device_upgrades_through_its_envelope_and_every_frame_is_sealed() {
    let (device, material) = encrypted_device();
    let state = test_state("admin", vec![device]);
    let runtime = state.agent_runtime.clone();
    let addr = serve(state).await;

    let (headers, nonce) = sealed_upgrade_headers(&material);
    let (mut client, response) = connect(addr, &headers).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(response.headers().get(TRANSPORT_HEADER).unwrap(), "1");
    assert!(response.headers().get("content-encoding").is_none());

    let mut app = AppCodec::open(&mut client, &material, &nonce).await;
    app.send(&mut client, r#"{"t":"subscribe","asid":"ses_1"}"#)
        .await;
    assert_eq!(
        app.recv(&mut client).await,
        json!({ "t": "subscribed", "asid": "ses_1" })
    );

    let event = status_event("ses_1", 9);
    runtime.publish_for_test(event.clone());
    let frame = app.recv(&mut client).await;
    let (name, data) = agent_event_record(&event);
    assert_eq!(frame["event"], name);
    assert_eq!(frame["data"], serde_json::from_str::<Value>(&data).unwrap());
    assert_eq!(frame["seq"], 9);

    app.send(&mut client, r#"{"t":"ping"}"#).await;
    assert_eq!(app.recv(&mut client).await, json!({ "t": "pong" }));
}

/// The device's token alone, without the envelope, is not how an encrypted
/// device opens anything -- the socket included.
#[tokio::test]
async fn a_sealed_device_cannot_upgrade_with_its_bare_token() {
    let (device, _) = encrypted_device();
    let addr = serve(test_state("admin", vec![device])).await;
    match connect(addr, &bearer()).await {
        Err(tungstenite::Error::Http(response)) => {
            assert_eq!(response.status(), StatusCode::FORBIDDEN)
        }
        other => panic!("expected 403, got {:?}", other.map(|(_, r)| r.status())),
    }
}

#[tokio::test]
async fn a_client_frame_out_of_sequence_closes_the_socket() {
    let (device, material) = encrypted_device();
    let addr = serve(test_state("admin", vec![device])).await;
    let (headers, nonce) = sealed_upgrade_headers(&material);
    let (mut client, _) = connect(addr, &headers).await.unwrap();
    let mut app = AppCodec::open(&mut client, &material, &nonce).await;

    // Validly sealed, but seq 1 where 0 is due.
    let skipped = app.seal_at(1, r#"{"t":"ping"}"#);
    send_text(&mut client, &skipped).await;
    assert_eq!(
        app.recv(&mut client).await,
        json!({ "t": "error", "code": "out_of_order" })
    );
    assert!(closes(&mut client).await);
}

#[tokio::test]
async fn only_subscribed_sessions_arrive_with_the_sse_name_and_data() {
    let state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    let runtime = state.agent_runtime.clone();
    let addr = serve(state).await;
    let (mut client, _) = connect(addr, &bearer()).await.unwrap();
    assert_eq!(next_json(&mut client).await["t"], "hello");

    send_text(&mut client, r#"{"t":"subscribe","asid":"ses_1"}"#).await;
    assert_eq!(next_json(&mut client).await["t"], "subscribed");

    runtime.publish_for_test(status_event("ses_other", 1));
    let wanted = status_event("ses_1", 2);
    runtime.publish_for_test(wanted.clone());

    let text = next_text(&mut client).await.unwrap();
    let (name, data) = agent_event_record(&wanted);
    // Byte for byte: the SSE `data:` line is a substring of the frame.
    assert_eq!(
        text,
        format!(r#"{{"t":"event","asid":"ses_1","seq":2,"event":"{name}","data":{data}}}"#)
    );
}

#[tokio::test]
async fn subscribe_all_delivers_every_session() {
    let state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    let runtime = state.agent_runtime.clone();
    let addr = serve(state).await;
    let (mut client, _) = connect(addr, &bearer()).await.unwrap();
    next_json(&mut client).await;

    send_text(&mut client, r#"{"t":"subscribe_all"}"#).await;
    assert_eq!(
        next_json(&mut client).await,
        json!({ "t": "subscribed", "all": true })
    );
    runtime.publish_for_test(status_event("ses_a", 1));
    runtime.publish_for_test(status_event("ses_b", 1));
    assert_eq!(next_json(&mut client).await["asid"], "ses_a");
    assert_eq!(next_json(&mut client).await["asid"], "ses_b");
}

#[tokio::test]
async fn an_unknown_frame_type_is_an_error_and_a_close() {
    let state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    let addr = serve(state).await;
    let (mut client, _) = connect(addr, &bearer()).await.unwrap();
    next_json(&mut client).await;

    send_text(&mut client, r#"{"t":"request","path":"/api/health"}"#).await;
    assert_eq!(
        next_json(&mut client).await,
        json!({ "t": "error", "code": "unknown_type" })
    );
    assert!(closes(&mut client).await);
}

/// Whether every socket slot is released within a generous bound, so a slow
/// CI machine does not turn a timer into a flake.
async fn slot_freed(state: &AppState) -> bool {
    for _ in 0..100 {
        if state.ws_connections.open_count() == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn fast_timings() -> WsTimings {
    WsTimings {
        heartbeat: Duration::from_millis(60),
        device_recheck: Duration::from_millis(50),
        send_timeout: Duration::from_secs(2),
    }
}

/// The socket twin of `revoking_a_device_closes_the_event_stream_it_already_had`:
/// revocation closes a socket the device already had, without a word.
#[tokio::test]
async fn revoking_a_device_closes_the_socket_it_already_had() {
    let mut state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    state.ws_connections = Arc::new(WsRegistry::with_timings(WsTimings {
        heartbeat: Duration::from_secs(60),
        ..fast_timings()
    }));
    let addr = serve(state.clone()).await;
    let (mut client, _) = connect(addr, &bearer()).await.unwrap();
    next_json(&mut client).await;

    state
        .devices
        .lock()
        .unwrap()
        .retain(|device| device.id != "phone-1");
    assert_eq!(
        next_text(&mut client).await,
        None,
        "a revoked device is closed without an error frame"
    );
    assert!(slot_freed(&state).await);
}

/// A client that stops answering pings is dropped after two of them.
#[tokio::test]
async fn a_peer_that_never_answers_pings_is_dropped() {
    let mut state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    state.ws_connections = Arc::new(WsRegistry::with_timings(fast_timings()));
    let addr = serve(state.clone()).await;
    let (mut client, _) = connect(addr, &bearer()).await.unwrap();
    next_json(&mut client).await;
    // Not reading means not answering: tungstenite only pongs while polled.
    assert!(
        slot_freed(&state).await,
        "the silent peer was never dropped"
    );
    assert!(closes(&mut client).await);
}

#[tokio::test]
async fn an_oversized_client_frame_is_refused_and_ends_the_socket() {
    let state = test_state("admin", vec![test_device("phone-1", TOKEN)]);
    let addr = serve(state.clone()).await;
    let (mut client, _) = connect(addr, &bearer()).await.unwrap();
    next_json(&mut client).await;
    let huge = format!(
        r#"{{"t":"ping","pad":"{}"}}"#,
        "x".repeat(MAX_INBOUND_FRAME_BYTES)
    );
    let _ = client.send(tungstenite::Message::Text(huge.into())).await;
    assert_eq!(
        next_text(&mut client).await.as_deref(),
        Some(r#"{"t":"error","code":"frame_too_large"}"#)
    );
    assert!(closes(&mut client).await);
}
