use std::sync::Arc;
use tokio::sync::broadcast;

use super::adapters::memory_mirror::MemoryMirror;
use super::adapters::opencode::{OpencodeDriver, OpencodeEndpoint, OpencodeRawEvent, OpencodeSseListener};
use super::domain::{AgentDomainEvent, AgentSessionId};
use super::ports::engine::AgentEnginePort;
use super::ports::mirror::SessionMirrorPort;
use super::use_cases::{InteractionService, PromptService, SessionService};

pub struct AgentManager {
    engine: Arc<dyn AgentEnginePort>,
    mirror: Arc<MemoryMirror>,
    session_service: Arc<SessionService>,
    prompt_service: Arc<PromptService>,
    interaction_service: Arc<InteractionService>,
    events_tx: broadcast::Sender<AgentDomainEvent>,
}

impl AgentManager {
    /// Attempt to discover and initialize available agent engines (such as OpenCode V2).
    pub async fn discover() -> Option<Self> {
        let endpoint = OpencodeEndpoint::discover().await?;
        let driver = Arc::new(OpencodeDriver::new(endpoint.clone()));
        let mirror = Arc::new(MemoryMirror::new());

        let session_service = Arc::new(SessionService::new(driver.clone(), mirror.clone()));
        let prompt_service = Arc::new(PromptService::new(driver.clone(), mirror.clone()));
        let interaction_service = Arc::new(InteractionService::new(driver.clone(), mirror.clone()));

        let (events_tx, _) = broadcast::channel(1024);

        // Setup SSE listener
        let (listener, mut sse_rx) = OpencodeSseListener::new(endpoint);
        listener.start();

        // Spawn background task to process incoming SSE events from the driver
        let mirror_clone = mirror.clone();
        let events_tx_clone = events_tx.clone();

        tokio::spawn(async move {
            while let Ok(raw_event) = sse_rx.recv().await {
                Self::handle_raw_event(raw_event, &mirror_clone, &events_tx_clone).await;
            }
        });

        eprintln!("AgentManager initialized with OpenCode engine");
        Some(Self {
            engine: driver,
            mirror,
            session_service,
            prompt_service,
            interaction_service,
            events_tx,
        })
    }

    pub fn is_available(&self) -> bool {
        true
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.events_tx.subscribe()
    }

    pub fn sessions(&self) -> &SessionService {
        &self.session_service
    }

    pub fn prompts(&self) -> &PromptService {
        &self.prompt_service
    }

    pub fn interactions(&self) -> &InteractionService {
        &self.interaction_service
    }

    async fn handle_raw_event(
        raw: OpencodeRawEvent,
        mirror: &Arc<MemoryMirror>,
        tx: &broadcast::Sender<AgentDomainEvent>,
    ) {
        let event_type = raw.event_type.as_str();
        let data = &raw.data;

        match event_type {
            "permission.asked" => {
                if let Some(session_id) = data.get("sessionID").and_then(serde_json::Value::as_str) {
                    let asid = AgentSessionId(session_id.to_string());
                    if let Some(req) = super::adapters::opencode::mapper::map_permission_request(data, &asid) {
                        let seq = mirror.add_permission(req.clone()).await;
                        let _ = tx.send(AgentDomainEvent::PermissionPending {
                            asid,
                            request: req,
                            seq,
                        });
                    }
                }
            }
            "permission.replied" => {
                if let Some(session_id) = data.get("sessionID").and_then(serde_json::Value::as_str) {
                    let asid = AgentSessionId(session_id.to_string());
                    if let Some(req_id) = data.get("requestID").and_then(serde_json::Value::as_str) {
                        let seq = mirror.resolve_permission(&asid, req_id).await;
                        let _ = tx.send(AgentDomainEvent::PermissionResolved {
                            asid,
                            request_id: req_id.to_string(),
                            seq,
                        });
                    }
                }
            }
            "form.created" => {
                if let Some(session_id) = data.get("sessionID").and_then(serde_json::Value::as_str) {
                    let asid = AgentSessionId(session_id.to_string());
                    if let Some(req) = super::adapters::opencode::mapper::map_form_request(data, &asid) {
                        let seq = mirror.add_form(req.clone()).await;
                        let _ = tx.send(AgentDomainEvent::FormPending {
                            asid,
                            request: req,
                            seq,
                        });
                    }
                }
            }
            "form.replied" | "form.cancelled" => {
                if let Some(session_id) = data.get("sessionID").and_then(serde_json::Value::as_str) {
                    let asid = AgentSessionId(session_id.to_string());
                    if let Some(form_id) = data.get("formID").or_else(|| data.get("id")).and_then(serde_json::Value::as_str) {
                        let seq = mirror.resolve_form(&asid, form_id).await;
                        let _ = tx.send(AgentDomainEvent::FormResolved {
                            asid,
                            form_id: form_id.to_string(),
                            seq,
                        });
                    }
                }
            }
            "session.text.delta" => {
                if let (Some(session_id), Some(msg_id), Some(delta)) = (
                    data.get("sessionID").and_then(serde_json::Value::as_str),
                    data.get("assistantMessageID").or_else(|| data.get("messageID")).and_then(serde_json::Value::as_str),
                    data.get("delta").and_then(serde_json::Value::as_str),
                ) {
                    let asid = AgentSessionId(session_id.to_string());
                    let ordinal = data.get("ordinal").and_then(serde_json::Value::as_u64).unwrap_or(0);
                    let item_id = format!("{msg_id}:{ordinal}");
                    let _ = mirror.append_text_delta(&asid, &item_id, delta, false).await;
                }
            }
            "session.reasoning.delta" => {
                if let (Some(session_id), Some(msg_id), Some(delta)) = (
                    data.get("sessionID").and_then(serde_json::Value::as_str),
                    data.get("assistantMessageID").or_else(|| data.get("messageID")).and_then(serde_json::Value::as_str),
                    data.get("delta").and_then(serde_json::Value::as_str),
                ) {
                    let asid = AgentSessionId(session_id.to_string());
                    let ordinal = data.get("ordinal").and_then(serde_json::Value::as_u64).unwrap_or(0);
                    let item_id = format!("{msg_id}:{ordinal}");
                    let _ = mirror.append_text_delta(&asid, &item_id, delta, true).await;
                }
            }
            _ => {}
        }
    }
}
