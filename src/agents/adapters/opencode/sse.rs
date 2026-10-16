use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::discovery::OpencodeEndpoint;

/// A single frame may legitimately be large -- a tool result inside
/// `message.part.updated` runs to tens of kilobytes -- but it is not unbounded.
/// A stream that never emits a blank line must not grow memory without limit.
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Frame in bytes, not HTTP chunks: UTF-8 is decoded only at a blank line.
/// Once oversized, discard through that same boundary (never parse its tail).
#[derive(Default)]
struct FrameBuffer {
    bytes: Vec<u8>,
    line_bytes: usize,
    line_is_cr: bool,
    discarding: bool,
}

impl FrameBuffer {
    fn push(&mut self, chunk: &[u8], sender: &broadcast::Sender<OpencodeRawEvent>) {
        for &byte in chunk {
            let boundary = byte == b'\n'
                && (self.line_bytes == 0 || (self.line_bytes == 1 && self.line_is_cr));
            if !self.discarding {
                if self.bytes.len() == MAX_FRAME_BYTES {
                    self.discarding = true;
                    self.bytes.clear();
                    tracing::warn!("oversized sse frame discarded");
                    Self::loss(sender, "event_frame_oversized");
                } else {
                    self.bytes.push(byte);
                }
            }
            if boundary {
                if !self.discarding {
                    match std::str::from_utf8(&self.bytes) {
                        Ok(block) => OpencodeSseListener::process_block(block, sender),
                        Err(err) => {
                            tracing::warn!(%err, "invalid utf8 sse frame discarded");
                            Self::loss(sender, "event_frame_invalid_utf8");
                        }
                    }
                }
                self.bytes.clear();
                self.discarding = false;
            }
            if byte == b'\n' {
                self.line_bytes = 0;
                self.line_is_cr = false;
            } else {
                self.line_bytes = self.line_bytes.saturating_add(1);
                self.line_is_cr = byte == b'\r';
            }
        }
    }

    fn loss(sender: &broadcast::Sender<OpencodeRawEvent>, reason: &str) {
        // Internal signal only; the pump maps this to the existing Resync.
        let _ = sender.send(OpencodeRawEvent {
            id: None,
            event_type: "gateway.stream.resync".to_string(),
            created: None,
            location: None,
            data: serde_json::json!({ "reason": reason }),
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpencodeRawEvent {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub event_type: String,
    /// The envelope timestamp in milliseconds. It is the only time a tool
    /// event carries, so it is kept rather than discarded.
    #[serde(default)]
    pub created: Option<u64>,
    /// The workspace the event belongs to, as `{directory, ...}`.
    #[serde(default)]
    pub location: Option<Value>,
    #[serde(default)]
    pub data: Value,
}

pub struct OpencodeSseListener {
    endpoint: OpencodeEndpoint,
    sender: broadcast::Sender<OpencodeRawEvent>,
    running: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl OpencodeSseListener {
    pub fn new(endpoint: OpencodeEndpoint) -> (Self, broadcast::Receiver<OpencodeRawEvent>) {
        let (sender, rx) = broadcast::channel(1024);
        let listener = Self {
            endpoint,
            sender,
            running: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        };
        (listener, rx)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<OpencodeRawEvent> {
        self.sender.subscribe()
    }

    /// True while the reader holds an open `/api/event` stream.
    /// Whether this listener is still meant to be reading.
    ///
    /// False once `stop` has been called, which is how a reader that finds its
    /// channel closed can tell an orderly hand-over from a real failure.
    pub fn is_running(&self) -> bool {
        !self.cancel.is_cancelled() && self.running.load(Ordering::SeqCst)
    }

    pub fn is_connected(&self) -> bool {
        !self.cancel.is_cancelled() && self.connected.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn start(&self) {
        // A stopped generation is retired; manager reattachment constructs a
        // fresh listener/token rather than reviving its old event pump.
        if self.cancel.is_cancelled() {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let endpoint = self.endpoint.clone();
        let sender = self.sender.clone();
        let running = self.running.clone();
        let connected = self.connected.clone();
        let cancel = self.cancel.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            let max_backoff = Duration::from_secs(8);
            let client = match reqwest::Client::builder()
                .tcp_keepalive(Duration::from_secs(15))
                .build()
            {
                Ok(client) => client,
                Err(err) => {
                    // Falling back to a default client silently is how a
                    // reader ends up with no keep-alive and nobody knowing.
                    tracing::warn!(%err, "sse client build failed, using defaults");
                    reqwest::Client::new()
                }
            };

            while running.load(Ordering::SeqCst) {
                let url = format!("{}/api/event", endpoint.url);
                let mut req = client.get(&url);
                if let Some(ref pwd) = endpoint.password {
                    req = req.basic_auth("opencode", Some(pwd));
                }

                let response = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    response = req.send() => response,
                };
                match response {
                    Ok(resp) if resp.status().is_success() => {
                        backoff = Duration::from_millis(500); // Reset backoff
                        connected.store(true, Ordering::SeqCst);
                        tracing::info!(url = %url, "opencode event stream connected");

                        let mut stream = resp.bytes_stream();
                        let mut buffer = FrameBuffer::default();

                        loop {
                            let chunk_res = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => break,
                                chunk = stream.next() => chunk,
                            };
                            let Some(chunk_res) = chunk_res else { break };
                            match chunk_res {
                                Ok(bytes) => buffer.push(&bytes, &sender),
                                Err(err) => {
                                    tracing::warn!(%err, "opencode event stream read failed");
                                    break;
                                }
                            }
                        }
                        connected.store(false, Ordering::SeqCst);
                        if !cancel.is_cancelled() {
                            // V2 is live-only. A reconnect cannot replay what
                            // arrived while disconnected, even at a boundary.
                            FrameBuffer::loss(&sender, "event_stream_disconnected");
                        }
                        tracing::warn!(url = %url, "opencode event stream ended");
                    }
                    Ok(resp) => {
                        connected.store(false, Ordering::SeqCst);
                        tracing::warn!(
                            url = %url,
                            status = %resp.status(),
                            "opencode event stream refused"
                        );
                    }
                    Err(err) => {
                        connected.store(false, Ordering::SeqCst);
                        tracing::warn!(url = %url, %err, "opencode event stream unreachable");
                    }
                }

                if !running.load(Ordering::SeqCst) {
                    break;
                }

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {},
                }
                backoff = (backoff * 2).min(max_backoff);
            }
            connected.store(false, Ordering::SeqCst);
            running.store(false, Ordering::SeqCst);
        });
    }

    pub fn stop(&self) {
        self.cancel.cancel();
        self.running.store(false, Ordering::SeqCst);
        self.connected.store(false, Ordering::SeqCst);
    }

    /// Parse one SSE block. Comment lines (`: heartbeat`) carry no `data:`
    /// prefix and are ignored; a `data:` payload split over several lines is
    /// joined with newlines, as the SSE specification requires.
    pub(crate) fn parse_block(block: &str) -> Option<OpencodeRawEvent> {
        let mut payload = String::new();
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            if line.starts_with(':') {
                continue;
            }
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        let payload = payload.trim();
        if payload.is_empty() {
            return None;
        }
        match serde_json::from_str::<OpencodeRawEvent>(payload) {
            Ok(event) => Some(event),
            Err(err) => {
                tracing::debug!(%err, "unparseable sse frame dropped");
                None
            }
        }
    }

    fn process_block(block: &str, sender: &broadcast::Sender<OpencodeRawEvent>) {
        if let Some(event) = Self::parse_block(block) {
            // No subscribers is not an error; it only means nothing is
            // watching this gateway yet.
            let _ = sender.send(event);
        }
    }
}

#[cfg(test)]
pub(crate) async fn read_test_request(socket: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(socket.read_u8().await.unwrap());
        assert!(headers.len() < 4096, "fake GET request headers bounded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn endpoint(url: String) -> OpencodeEndpoint {
        OpencodeEndpoint {
            url,
            password: None,
            version: None,
            pid: None,
        }
    }

    fn unicode_frames(ending: &str) -> Vec<u8> {
        format!(
            "data: {{\"type\":\"session.text.delta\",\"data\":{{\"delta\":\"你好🙂\"}}}}{ending}{ending}data: {{\"type\":\"session.idle\",\"data\":{{}}}}{ending}{ending}"
        ).into_bytes()
    }

    #[test]
    fn unicode_and_concatenated_frames_survive_every_byte_split() {
        for ending in ["\n", "\r\n"] {
            let frames = unicode_frames(ending);
            for split in 0..=frames.len() {
                let (tx, mut rx) = broadcast::channel(4);
                let mut buffer = FrameBuffer::default();
                buffer.push(&frames[..split], &tx);
                buffer.push(&frames[split..], &tx);
                assert_eq!(rx.try_recv().unwrap().data["delta"], "你好🙂");
                assert_eq!(rx.try_recv().unwrap().event_type, "session.idle");
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[test]
    fn oversized_frames_recover_only_at_boundary_even_with_delimiter_in_chunk() {
        for split_oversize in [false, true] {
            let (tx, mut rx) = broadcast::channel(8);
            let mut buffer = FrameBuffer::default();
            let mut oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
            oversized.extend_from_slice(b"data: {\"type\":\"must.not.parse\"}\r\n\r\n");
            oversized.extend_from_slice(&unicode_frames("\r\n"));
            if split_oversize {
                buffer.push(&oversized[..MAX_FRAME_BYTES + 1], &tx);
                assert!(buffer.bytes.is_empty());
                buffer.push(&oversized[MAX_FRAME_BYTES + 1..], &tx);
            } else {
                buffer.push(&oversized, &tx);
            }
            assert_eq!(rx.try_recv().unwrap().event_type, "gateway.stream.resync");
            assert_eq!(rx.try_recv().unwrap().data["delta"], "你好🙂");
            assert_eq!(rx.try_recv().unwrap().event_type, "session.idle");
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn invalid_utf8_resyncs_without_losing_the_following_frame() {
        let (tx, mut rx) = broadcast::channel(4);
        let mut buffer = FrameBuffer::default();
        buffer.push(b"data: \xff\n\n", &tx);
        buffer.push(&unicode_frames("\n"), &tx);
        assert_eq!(
            rx.try_recv().unwrap().data["reason"],
            "event_frame_invalid_utf8"
        );
        assert_eq!(rx.try_recv().unwrap().data["delta"], "你好🙂");
        assert_eq!(rx.try_recv().unwrap().event_type, "session.idle");
    }

    #[tokio::test]
    async fn live_only_http_disconnect_announces_resync_after_complete_frames() {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", tcp.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = tcp.accept().await.unwrap();
            read_test_request(&mut socket).await;
            let body = unicode_frames("\n");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        let (listener, mut rx) = OpencodeSseListener::new(endpoint(url));
        listener.start();
        tokio::time::timeout(Duration::from_secs(2), async {
            assert_eq!(rx.recv().await.unwrap().data["delta"], "你好🙂");
            assert_eq!(rx.recv().await.unwrap().event_type, "session.idle");
            assert_eq!(
                rx.recv().await.unwrap().data["reason"],
                "event_stream_disconnected"
            );
        })
        .await
        .unwrap();
        listener.stop();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn real_http_one_byte_chunks_preserve_unicode() {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", tcp.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = tcp.accept().await.unwrap();
            read_test_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            for byte in unicode_frames("\r\n") {
                socket
                    .write_all(&[b'1', b'\r', b'\n', byte, b'\r', b'\n'])
                    .await
                    .unwrap();
            }
            socket.flush().await.unwrap();
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
        });
        let (listener, mut rx) = OpencodeSseListener::new(endpoint(url));
        listener.start();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.data["delta"], "你好🙂");
        assert_eq!(rx.recv().await.unwrap().event_type, "session.idle");
        listener.stop();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stop_cancels_idle_handshake_stream_and_backoff() {
        for response in [None, Some("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"), Some("HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")] {
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", tcp.local_addr().unwrap());
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = tcp.accept().await.unwrap();
                read_test_request(&mut socket).await;
                if let Some(response) = response {
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.flush().await.unwrap();
                }
                ready_tx.send(()).unwrap();
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
            });
            let (listener, _rx) = OpencodeSseListener::new(endpoint(url));
            listener.start();
            ready_rx.await.unwrap();
            // Let headers reach the reader, so 503 exercises its backoff
            // rather than merely cancelling the still-pending handshake.
            tokio::time::sleep(Duration::from_millis(20)).await;
            listener.stop();
            // With only the task's clone left, Weak proves the reader exited,
            // including while in backoff (which holds no active socket).
            let weak = Arc::downgrade(&listener.running);
            drop(listener);
            tokio::time::timeout(Duration::from_millis(200), async {
                while weak.upgrade().is_some() { tokio::task::yield_now().await; }
            }).await.expect("reader releases state immediately");
            tokio::time::timeout(Duration::from_secs(1), server).await.unwrap().unwrap();
        }
    }

    #[test]
    fn a_heartbeat_comment_is_not_an_event() {
        assert!(OpencodeSseListener::parse_block(": heartbeat").is_none());
        assert!(OpencodeSseListener::parse_block("").is_none());
        assert!(OpencodeSseListener::parse_block("data:").is_none());
    }

    #[test]
    fn a_single_line_frame_parses() {
        let block = r#"data: {"id":"evt_1","created":1789640956167,"type":"session.created","location":{"directory":"/tmp"},"data":{"sessionID":"ses_1","title":"probe"}}"#;
        let event = OpencodeSseListener::parse_block(block).expect("frame parses");
        assert_eq!(event.event_type, "session.created");
        assert_eq!(event.created, Some(1789640956167));
        assert_eq!(
            event
                .location
                .as_ref()
                .and_then(|l| l.get("directory"))
                .and_then(Value::as_str),
            Some("/tmp")
        );
        assert_eq!(
            event.data.get("sessionID").and_then(Value::as_str),
            Some("ses_1")
        );
    }

    #[test]
    fn a_multi_line_data_payload_is_joined_with_newlines() {
        // SSE splits a payload containing newlines across several data lines.
        let block = "event: message\ndata: {\"type\":\"session.text.delta\",\"data\":{\n\ndata: \"sessionID\":\"ses_1\",\"delta\":\"a\\nb\"}}";
        // The blank line inside the block cannot occur in a real frame, so the
        // realistic form is two consecutive data lines:
        let block = block.replace("\n\ndata: ", "\ndata: ");
        let event = OpencodeSseListener::parse_block(&block).expect("frame parses");
        assert_eq!(event.event_type, "session.text.delta");
        assert_eq!(
            event.data.get("delta").and_then(Value::as_str),
            Some("a\nb")
        );
    }

    #[test]
    fn a_comment_between_data_lines_is_skipped() {
        let block =
            ": ping\ndata: {\"type\":\"session.idle\",\"data\":{\"sessionID\":\"ses_2\"}}\n: ping";
        let event = OpencodeSseListener::parse_block(block).expect("frame parses");
        assert_eq!(event.event_type, "session.idle");
    }

    #[test]
    fn a_malformed_frame_is_dropped_rather_than_panicking() {
        assert!(OpencodeSseListener::parse_block("data: {not json").is_none());
        assert!(OpencodeSseListener::parse_block("data: {\"no\":\"type\"}").is_none());
    }
}
