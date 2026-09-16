use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

use super::discovery::OpencodeEndpoint;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpencodeRawEvent {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub data: Value,
}

pub struct OpencodeSseListener {
    endpoint: OpencodeEndpoint,
    sender: broadcast::Sender<OpencodeRawEvent>,
    running: Arc<AtomicBool>,
}

impl OpencodeSseListener {
    pub fn new(endpoint: OpencodeEndpoint) -> (Self, broadcast::Receiver<OpencodeRawEvent>) {
        let (sender, rx) = broadcast::channel(1024);
        let listener = Self {
            endpoint,
            sender,
            running: Arc::new(AtomicBool::new(false)),
        };
        (listener, rx)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<OpencodeRawEvent> {
        self.sender.subscribe()
    }

    pub fn start(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let endpoint = self.endpoint.clone();
        let sender = self.sender.clone();
        let running = self.running.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            let max_backoff = Duration::from_secs(8);
            let client = reqwest::Client::builder()
                .tcp_keepalive(Duration::from_secs(15))
                .build()
                .unwrap_or_default();

            while running.load(Ordering::SeqCst) {
                let url = format!("{}/api/event", endpoint.url);
                let mut req = client.get(&url);
                if let Some(ref pwd) = endpoint.password {
                    req = req.basic_auth("opencode", Some(pwd));
                }

                match req.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        backoff = Duration::from_millis(500); // Reset backoff

                        let mut stream = resp.bytes_stream();
                        let mut buffer = String::new();

                        while let Some(chunk_res) = stream.next().await {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }
                            match chunk_res {
                                Ok(bytes) => {
                                    if let Ok(text) = std::str::from_utf8(&bytes) {
                                        buffer.push_str(text);
                                        while let Some(pos) = buffer.find("\n\n") {
                                            let message_block = buffer[..pos].to_string();
                                            buffer.drain(..pos + 2);
                                            Self::process_block(&message_block, &sender);
                                        }
                                    }
                                }
                                Err(_) => {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }

                if !running.load(Ordering::SeqCst) {
                    break;
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        });
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn process_block(block: &str, sender: &broadcast::Sender<OpencodeRawEvent>) {
        for line in block.lines() {
            let line = line.trim();
            if let Some(data_str) = line.strip_prefix("data:") {
                let data_str = data_str.trim();
                if data_str.is_empty() {
                    continue;
                }
                if let Ok(raw_event) = serde_json::from_str::<OpencodeRawEvent>(data_str) {
                    let _ = sender.send(raw_event);
                }
            }
        }
    }
}
