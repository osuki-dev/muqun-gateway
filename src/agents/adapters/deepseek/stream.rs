use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::SinkExt;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::endpoint::DeepseekEndpoint;
use crate::agents::domain::AgentDomainEvent;

/// Messages sent from Gateway client to DeepSeek Harness stream multiplexer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RemoteStreamClientMessage {
    #[serde(rename = "open")]
    Open {
        #[serde(rename = "streamId")]
        stream_id: String,
        endpoint: String,
        payload: Value,
    },
    #[serde(rename = "cancel")]
    Cancel {
        #[serde(rename = "streamId")]
        stream_id: String,
    },
    #[serde(rename = "item")]
    Item {
        #[serde(rename = "streamId")]
        stream_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
}

/// Messages received by Gateway client from DeepSeek Harness stream multiplexer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum RemoteStreamServerMessage {
    #[serde(rename = "item")]
    Item {
        #[serde(rename = "streamId")]
        stream_id: String,
        #[serde(default)]
        value: Value,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(rename = "streamId")]
        stream_id: String,
        error: Value,
    },
    #[serde(rename = "end")]
    End {
        #[serde(rename = "streamId")]
        stream_id: String,
    },
}

/// Parse a raw text WebSocket frame into a `RemoteStreamServerMessage`.
pub fn parse_stream_server_message(text: &str) -> Result<RemoteStreamServerMessage, String> {
    serde_json::from_str::<RemoteStreamServerMessage>(text)
        .map_err(|e| format!("Failed to parse stream message: {e}"))
}

/// Format an open stream client request message.
pub fn format_open_stream(stream_id: &str, endpoint: &str, payload: Value) -> String {
    let msg = RemoteStreamClientMessage::Open {
        stream_id: stream_id.to_string(),
        endpoint: endpoint.to_string(),
        payload,
    };
    serde_json::to_string(&msg).unwrap_or_default()
}

pub struct DeepseekStreamListener {
    endpoint: DeepseekEndpoint,
    running: Arc<AtomicBool>,
}

impl DeepseekStreamListener {
    pub fn new(endpoint: DeepseekEndpoint) -> Self {
        Self {
            endpoint,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Connect to DeepSeek stream mux and listen for session events.
    pub fn start(
        &self,
        _events_tx: broadcast::Sender<AgentDomainEvent>,
        session_id: Option<String>,
    ) {
        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();
        let ws_url = self.endpoint.ws_url.clone();
        let token = self.endpoint.token.clone();
        let auth_cookie = self.endpoint.auth_cookie();
        let authority = self.endpoint.authority();

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            while running.load(Ordering::SeqCst) {
                tracing::info!(%ws_url, "connecting to DeepSeek Harness WebSocket mux");

                // Formulate WS URL with auth if token present
                let connect_url = if let Some(ref tok) = token {
                    if ws_url.contains('?') {
                        format!("{ws_url}&token={tok}")
                    } else {
                        format!("{ws_url}?token={tok}")
                    }
                } else {
                    ws_url.clone()
                };

                let req_result = connect_url
                    .into_client_request()
                    .map_err(|e| e.to_string())
                    .map(|mut req| {
                        if let Some(ref cookie) = auth_cookie {
                            if let Ok(val) = HeaderValue::from_str(cookie) {
                                req.headers_mut().insert("Cookie", val);
                            }
                        }
                        if let Ok(val) = HeaderValue::from_str(&authority) {
                            req.headers_mut().insert("Host", val);
                        }
                        req
                    });

                let client_req = match req_result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(%e, "failed to build WebSocket request");
                        break;
                    }
                };

                match connect_async(client_req).await {
                    Ok((ws_stream, _)) => {
                        tracing::info!("connected to DeepSeek Harness WebSocket mux");
                        backoff = Duration::from_millis(500);
                        let (mut write, mut read) = ws_stream.split();

                        // Open session.follow stream if session_id is provided
                        let stream_id = format!("stream_{}", Uuid::new_v4().simple());
                        if let Some(ref sid) = session_id {
                            let open_payload = json!({
                                "args": {
                                    "request": {
                                        "address": {
                                            "kind": "session",
                                            "sessionId": sid
                                        }
                                    }
                                }
                            });
                            let open_frame =
                                format_open_stream(&stream_id, "session/follow", open_payload);
                            if let Err(e) = write.send(Message::Text(open_frame.into())).await {
                                tracing::warn!(%e, "failed to send session.follow open frame");
                                continue;
                            }
                        }

                        while running.load(Ordering::SeqCst) {
                            match read.next().await {
                                Some(Ok(Message::Text(text))) => {
                                    match parse_stream_server_message(&text) {
                                        Ok(RemoteStreamServerMessage::Item { value, .. }) => {
                                            tracing::trace!(?value, "received remote stream item");
                                        }
                                        Ok(RemoteStreamServerMessage::Error { error, .. }) => {
                                            tracing::warn!(?error, "stream reported error");
                                        }
                                        Ok(RemoteStreamServerMessage::End { .. }) => {
                                            tracing::info!("stream ended by host");
                                            break;
                                        }
                                        Err(e) => {
                                            tracing::debug!(%e, %text, "unrecognized text frame");
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(_))) => {
                                    tracing::info!("server closed stream websocket");
                                    break;
                                }
                                Some(Err(e)) => {
                                    tracing::warn!(%e, "websocket read error");
                                    break;
                                }
                                None => break,
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(%e, "failed to connect to DeepSeek Harness WebSocket mux");
                    }
                }

                if running.load(Ordering::SeqCst) {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }
            tracing::info!("DeepSeek stream listener stopped");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_open_stream_message() {
        let msg = format_open_stream(
            "stream_1",
            "session/follow",
            json!({ "args": { "sessionId": "s_1" } }),
        );
        let parsed: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["type"], "open");
        assert_eq!(parsed["streamId"], "stream_1");
        assert_eq!(parsed["endpoint"], "session/follow");
        assert_eq!(parsed["payload"]["args"]["sessionId"], "s_1");
    }

    #[test]
    fn parses_server_stream_item() {
        let raw = json!({
            "type": "item",
            "streamId": "stream_1",
            "value": { "type": "snapshot", "header": { "version": 1 } }
        });
        let msg = parse_stream_server_message(&serde_json::to_string(&raw).unwrap()).unwrap();
        match msg {
            RemoteStreamServerMessage::Item { stream_id, value } => {
                assert_eq!(stream_id, "stream_1");
                assert_eq!(value["type"], "snapshot");
            }
            _ => panic!("Expected Item variant"),
        }
    }
}
