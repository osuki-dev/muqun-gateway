use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

use super::discovery::OpencodeEndpoint;

/// A single frame may legitimately be large -- a tool result inside
/// `message.part.updated` runs to tens of kilobytes -- but it is not unbounded.
/// A stream that never emits a blank line must not grow memory without limit.
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
}

impl OpencodeSseListener {
    pub fn new(endpoint: OpencodeEndpoint) -> (Self, broadcast::Receiver<OpencodeRawEvent>) {
        let (sender, rx) = broadcast::channel(1024);
        let listener = Self {
            endpoint,
            sender,
            running: Arc::new(AtomicBool::new(false)),
            connected: Arc::new(AtomicBool::new(false)),
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
        self.running.load(Ordering::SeqCst)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn start(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let endpoint = self.endpoint.clone();
        let sender = self.sender.clone();
        let running = self.running.clone();
        let connected = self.connected.clone();

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

                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        backoff = Duration::from_millis(500); // Reset backoff
                        connected.store(true, Ordering::SeqCst);
                        tracing::info!(url = %url, "opencode event stream connected");

                        let mut stream = resp.bytes_stream();
                        let mut buffer = String::new();

                        while let Some(chunk_res) = stream.next().await {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }
                            match chunk_res {
                                Ok(bytes) => match std::str::from_utf8(&bytes) {
                                    Ok(text) => {
                                        buffer.push_str(text);
                                        while let Some(pos) = buffer.find("\n\n") {
                                            let message_block = buffer[..pos].to_string();
                                            buffer.drain(..pos + 2);
                                            Self::process_block(&message_block, &sender);
                                        }
                                        if buffer.len() > MAX_FRAME_BYTES {
                                            tracing::warn!(
                                                bytes = buffer.len(),
                                                "oversized sse frame discarded"
                                            );
                                            buffer.clear();
                                        }
                                    }
                                    Err(err) => {
                                        // A chunk can split a multi-byte
                                        // character; only give up if the whole
                                        // buffer is unusable.
                                        tracing::debug!(%err, "non-utf8 sse chunk skipped");
                                    }
                                },
                                Err(err) => {
                                    tracing::warn!(%err, "opencode event stream read failed");
                                    break;
                                }
                            }
                        }
                        connected.store(false, Ordering::SeqCst);
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

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
            connected.store(false, Ordering::SeqCst);
        });
    }

    pub fn stop(&self) {
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
mod tests {
    use super::*;

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
            event.location.as_ref().and_then(|l| l.get("directory")).and_then(Value::as_str),
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
        let block = ": ping\ndata: {\"type\":\"session.idle\",\"data\":{\"sessionID\":\"ses_2\"}}\n: ping";
        let event = OpencodeSseListener::parse_block(block).expect("frame parses");
        assert_eq!(event.event_type, "session.idle");
    }

    #[test]
    fn a_malformed_frame_is_dropped_rather_than_panicking() {
        assert!(OpencodeSseListener::parse_block("data: {not json").is_none());
        assert!(OpencodeSseListener::parse_block("data: {\"no\":\"type\"}").is_none());
    }
}
