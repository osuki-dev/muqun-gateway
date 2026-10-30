use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use super::mapper::{self, Permission, Projection};
use super::{AcpClient, AcpEvent};
use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::domain::*;
use crate::agents::ports::agent::{
    AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem,
};
use crate::agents::ports::mirror::{AgentSessionSnapshot, SessionMirrorPort};
use std::marker::PhantomData;

/// Native launch and verified differences; protocol lifecycle is shared.
pub trait AcpProvider: Send + Sync + 'static {
    const KIND: &'static str;
    const LABEL: &'static str;
    const ARGS: &'static [&'static str];
    const REQUIRE_MESSAGE_ID: bool;
    const MODEL_SELECTION: bool;
    fn safe_error_message(_: &Value) -> Option<&'static str> {
        None
    }
}

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 16;
const MAX_LIST_PAGES: usize = 64;

struct SessionState {
    info: AgentSessionInfo,
    catalog: AgentCatalog,
    config: Value,
    projection: Projection,
    permissions: HashMap<String, Permission>,
}

struct Session {
    client: Arc<AcpClient>,
    state: Mutex<SessionState>,
    busy: AtomicBool,
    connected: AtomicBool,
    completions: mpsc::UnboundedSender<Result<Value, String>>,
    finished: CancellationToken,
}

struct Admission<'a> {
    busy: &'a AtomicBool,
    handed_off: bool,
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if !self.handed_off {
            self.busy.store(false, Ordering::Release);
        }
    }
}

pub struct AcpDriver<P: AcpProvider> {
    provider: PhantomData<P>,
    binary: String,
    default_directory: String,
    mirror: Arc<MemoryMirror>,
    tx: broadcast::Sender<AgentDomainEvent>,
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
    connections: StdMutex<Vec<Weak<AcpClient>>>,
    connection_admission: Mutex<()>,
    starting: tokio::sync::RwLock<()>,
    mutations: Mutex<()>,
    stop: CancellationToken,
}

fn unsupported<'a, T: Send + 'a>(feature: &'static str) -> AgentFuture<'a, T> {
    Box::pin(async move { Err(AgentError::Unsupported(feature.into())) })
}

fn protocol(message: &str) -> AgentError {
    AgentError::Protocol(message.into())
}

impl<P: AcpProvider> AcpDriver<P> {
    pub fn new(
        binary: String,
        default_directory: String,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider: PhantomData,
            binary,
            default_directory,
            mirror,
            tx,
            sessions: StdMutex::new(HashMap::new()),
            connections: StdMutex::new(Vec::new()),
            connection_admission: Mutex::new(()),
            starting: tokio::sync::RwLock::new(()),
            mutations: Mutex::new(()),
            stop: CancellationToken::new(),
        })
    }

    /// Discovery verifies the binary. ACP is initialized only on an explicit read or mutation.
    pub fn is_connected(&self) -> bool {
        !self.stop.is_cancelled()
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }

    pub fn has_live_stream(&self) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .any(|session| session.connected.load(Ordering::Acquire))
    }

    pub async fn shutdown(&self) {
        self.stop();
        // Initialization registers every private child before releasing this
        // gate, including short-lived listing clients and unfinished creates.
        let _starting = self.starting.write().await;
        let clients: Vec<_> = self
            .connections
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        futures::future::join_all(clients.iter().map(|client| client.shutdown())).await;
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        futures::future::join_all(sessions.iter().map(|session| async move {
            session.client.shutdown().await;
            let _ =
                tokio::time::timeout(Duration::from_secs(5), session.finished.cancelled()).await;
        }))
        .await;
    }

    /// Only catalogs actually received from native sessions contribute choices.
    pub async fn observed_catalog(&self) -> AgentCatalog {
        let mut sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        let mut models: BTreeMap<(String, String), ModelInfo> = BTreeMap::new();
        for (_, session) in sessions {
            if !session.connected.load(Ordering::Acquire) {
                continue;
            }
            for model in &session.state.lock().await.catalog.models {
                let current = models
                    .entry((model.provider_id.clone(), model.id.clone()))
                    .or_insert_with(|| model.clone());
                if let Some(variants) = &model.variants {
                    let mut known: BTreeMap<_, _> = current
                        .variants
                        .take()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|variant| (variant.id.clone(), variant))
                        .collect();
                    for variant in variants {
                        known
                            .entry(variant.id.clone())
                            .or_insert_with(|| variant.clone());
                    }
                    current.variants = Some(known.into_values().collect());
                }
            }
        }
        AgentCatalog {
            models: models.into_values().collect(),
            ..Default::default()
        }
    }

    async fn select_config(
        &self,
        session: &Session,
        config_id: &str,
        value: &str,
    ) -> Result<(), AgentError> {
        let mut state = session.state.lock().await;
        let option = mapper::config_option(&state.config, config_id)
            .ok_or_else(|| AgentError::Unsupported(format!("ACP {config_id} selection")))?;
        if option["currentValue"].as_str() == Some(value) {
            return Ok(());
        }
        if !option["options"].as_array().is_some_and(|options| {
            options
                .iter()
                .any(|option| option["value"].as_str() == Some(value))
        }) {
            return Err(AgentError::InvalidRequest(format!(
                "ACP {config_id} choice is not offered by this session"
            )));
        }
        let previous_provider = state
            .info
            .model
            .as_ref()
            .map(|model| model.provider_id.clone());
        let response = session
            .client
            .request_with_timeout(
                "session/set_config_option",
                json!({
                    "sessionId": state.info.asid.0, "configId": config_id, "value": value
                }),
                RPC_TIMEOUT,
            )
            .await
            .map_err(AgentError::RequestFailed)?;
        if !response["configOptions"].is_array() {
            return Err(protocol(
                "ACP config selection returned no configOptions; refresh before retrying",
            ));
        }
        let update = json!({"sessionUpdate":"config_option_update","configOptions":response["configOptions"]});
        Self::reduce(&mut state, &update)?;
        let info = state.info.clone();
        let seq = self.mirror.update_session(info.clone()).await;
        Self::emit(
            &self.mirror,
            &self.tx,
            AgentDomainEvent::SessionUpdated {
                asid: info.asid.clone(),
                info: Box::new(info),
                seq,
            },
        );
        if config_id != "provider"
            && state
                .info
                .model
                .as_ref()
                .map(|model| model.provider_id.clone())
                != previous_provider
        {
            return Err(protocol("ACP changed the provider during model or effort selection; native state was retained, refresh before retrying"));
        }
        if mapper::config_option(&state.config, config_id)
            .and_then(|option| option["currentValue"].as_str())
            != Some(value)
        {
            return Err(protocol(
                "ACP did not confirm the requested config value; refresh before retrying",
            ));
        }
        Ok(())
    }

    async fn select_model(&self, session: &Session, model: &ModelRef) -> Result<(), AgentError> {
        if !P::MODEL_SELECTION {
            return Err(AgentError::Unsupported("ACP model selection".into()));
        }
        if !session.connected.load(Ordering::Acquire) {
            return Err(AgentError::NotAvailable("ACP connection ended".into()));
        }
        if session
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentError::NotAvailable(
                "ACP session is busy; model changes require an idle session".into(),
            ));
        }
        let _admission = Admission {
            busy: &session.busy,
            handed_off: false,
        };
        self.select_config(session, "provider", &model.provider_id)
            .await?;
        self.select_config(session, "model", &model.model_id)
            .await?;
        if let Some(effort) = &model.variant {
            self.select_config(session, "effort", effort).await?;
        }
        Ok(())
    }

    async fn initialized(
        &self,
        directory: &str,
    ) -> Result<(Arc<AcpClient>, broadcast::Receiver<AcpEvent>, Value), AgentError> {
        let _starting = self.starting.read().await;
        if self.stop.is_cancelled() {
            return Err(AgentError::NotAvailable("ACP runtime stopped".into()));
        }
        // Reserve two short-lived listing connections beyond the session cap.
        // Serialize spawn admission only; prompts and initialization stay concurrent.
        let admission = self.connection_admission.lock().await;
        {
            let mut connections = self.connections.lock().unwrap();
            connections.retain(|client| client.strong_count() > 0);
            if connections.len() >= MAX_CONNECTIONS + 2 {
                return Err(AgentError::NotAvailable(
                    "ACP connection capacity exhausted".into(),
                ));
            }
        }
        let client = AcpClient::spawn_with_error_mapper(
            &self.binary,
            P::ARGS,
            directory,
            P::LABEL,
            P::safe_error_message,
        )
        .await
        .map_err(AgentError::NotAvailable)?;
        {
            let mut connections = self.connections.lock().unwrap();
            connections.retain(|client| client.strong_count() > 0);
            connections.push(Arc::downgrade(&client));
        }
        drop(admission);
        let events = client.subscribe();
        let initialize = client.request_with_timeout("initialize", json!({
            "protocolVersion": 1,
            "clientInfo": { "name": "muqun-gateway", "version": env!("CARGO_PKG_VERSION") },
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false }
        }), RPC_TIMEOUT);
        let result = tokio::select! {
            _ = self.stop.cancelled() => Err("ACP runtime stopped".into()),
            result = initialize => result,
        };
        match result {
            Ok(value) if value["protocolVersion"].as_u64() == Some(1) => {
                Ok((client, events, value["agentCapabilities"].clone()))
            }
            result => {
                client.shutdown().await;
                Err(match result {
                    Ok(_) => protocol("ACP did not negotiate ACP protocol 1"),
                    Err(error) => AgentError::Protocol(error),
                })
            }
        }
    }

    fn retained(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    async fn native_sessions(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        let (client, _events, capabilities) = self.initialized(&self.default_directory).await?;
        if !capabilities
            .pointer("/sessionCapabilities/list")
            .is_some_and(Value::is_object)
        {
            client.shutdown().await;
            let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
            let mut retained = Vec::new();
            for session in sessions {
                let info = session.state.lock().await.info.clone();
                if directory.is_none_or(|cwd| info.directory.as_deref() == Some(cwd)) {
                    retained.push(info);
                }
            }
            return Ok(retained);
        }
        let result = async {
            let mut cursor = None;
            let mut seen = HashSet::new();
            let mut sessions = Vec::new();
            let mut listed = HashSet::new();
            let mut observed_bytes: usize = 0;
            for _ in 0..MAX_LIST_PAGES {
                let mut params = json!({});
                if let Some(directory) = directory {
                    params["cwd"] = json!(directory);
                }
                if let Some(cursor) = &cursor {
                    params["cursor"] = json!(cursor);
                }
                let value = client
                    .request_with_timeout("session/list", params, RPC_TIMEOUT)
                    .await
                    .map_err(AgentError::RequestFailed)?;
                observed_bytes = observed_bytes.saturating_add(
                    serde_json::to_vec(&value)
                        .map(|bytes| bytes.len())
                        .unwrap_or(usize::MAX),
                );
                if observed_bytes > 8 * 1024 * 1024 {
                    return Err(AgentError::NotAvailable(
                        "ACP session listing exceeded its byte budget".into(),
                    ));
                }
                let rows = value["sessions"]
                    .as_array()
                    .ok_or_else(|| protocol("ACP list missing sessions"))?;
                for row in rows {
                    let id = row["sessionId"]
                        .as_str()
                        .ok_or_else(|| protocol("ACP listed session missing sessionId"))?;
                    let cwd = row["cwd"]
                        .as_str()
                        .ok_or_else(|| protocol("ACP listed session missing cwd"))?;
                    if !listed.insert(id.to_owned()) {
                        continue;
                    }
                    if sessions.len() >= 4096 {
                        return Err(AgentError::NotAvailable(
                            "ACP session listing exceeded its session budget".into(),
                        ));
                    }
                    let mut info = mapper::session(id, cwd, row);
                    // Listing does not prove ownership of the native runtime or that it is idle.
                    info.status = AgentSessionStatus::Unknown;
                    if let Some(known) = self.retained(id) {
                        info = known.state.lock().await.info.clone();
                    }
                    sessions.push(info);
                }
                let Some(next) = value["nextCursor"].as_str() else {
                    return Ok(sessions);
                };
                if next.is_empty() || !seen.insert(next.to_owned()) {
                    return Err(protocol("ACP session cursor did not advance"));
                }
                cursor = Some(next.to_owned());
            }
            Err(AgentError::NotAvailable(
                "ACP session listing exceeded its page budget".into(),
            ))
        }
        .await;
        client.shutdown().await;
        result
    }

    async fn ensure_loaded(&self, id: &str) -> Result<Arc<Session>, AgentError> {
        if let Some(session) = self.retained(id) {
            if session.connected.load(Ordering::Acquire) {
                return Ok(session);
            }
            return Err(AgentError::NotAvailable(
                "ACP connection ended; refresh the session explicitly before sending again".into(),
            ));
        }
        let _mutation = self.mutations.lock().await;
        if let Some(session) = self.retained(id) {
            if session.connected.load(Ordering::Acquire) {
                return Ok(session);
            }
            return Err(AgentError::NotAvailable(
                "ACP connection ended; refresh the session explicitly before sending again".into(),
            ));
        }
        let info = self
            .native_sessions(None)
            .await?
            .into_iter()
            .find(|info| info.asid.0 == id)
            .ok_or_else(|| AgentError::SessionNotFound(id.into()))?;
        let directory = info
            .directory
            .clone()
            .ok_or_else(|| protocol("ACP session has no working directory"))?;
        self.open(&directory, Some(info)).await
    }

    async fn open(
        &self,
        directory: &str,
        existing: Option<AgentSessionInfo>,
    ) -> Result<Arc<Session>, AgentError> {
        if self.sessions.lock().unwrap().len() >= MAX_CONNECTIONS {
            return Err(AgentError::NotAvailable(
                "ACP active-session capacity exhausted".into(),
            ));
        }
        let id = existing.as_ref().map(|info| info.asid.0.clone());
        let revision = match &id {
            Some(id) => {
                self.mirror
                    .snapshot_read_revision(&AgentSessionId::from(id.as_str()))
                    .await
            }
            None => (None, None),
        };
        let (client, mut events, capabilities) = self.initialized(directory).await?;
        if id.is_some() && capabilities["loadSession"].as_bool() != Some(true) {
            client.shutdown().await;
            return Err(AgentError::Unsupported(
                "ACP native session history loading".into(),
            ));
        }
        let method = if id.is_some() {
            "session/load"
        } else {
            "session/new"
        };
        let mut params = json!({ "cwd": directory, "mcpServers": [] });
        if let Some(id) = &id {
            params["sessionId"] = json!(id);
        }
        let request = client
            .start_request(method, params)
            .await
            .map_err(AgentError::RequestFailed)?;
        let response = request.wait_with_timeout(RPC_TIMEOUT);
        tokio::pin!(response);
        let mut replay = Vec::new();
        let mut replay_bytes: usize = 0;
        // Drain as the full load replays, instead of leaving the bounded transport
        // subscription full while waiting for the response.
        let value = loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(AcpEvent::Notification { method, params }) if method == "session/update" => {
                        replay_bytes = replay_bytes.saturating_add(serde_json::to_vec(&params).map(|bytes| bytes.len()).unwrap_or(usize::MAX));
                        if replay_bytes > 32 * 1024 * 1024 || replay.len() >= 100_000 {
                            client.shutdown().await;
                            return Err(AgentError::NotAvailable("ACP whole-history replay exceeds the Gateway budget; open this session in the agent's terminal".into()));
                        }
                        replay.push(params);
                    },
                    Ok(AcpEvent::Disconnected { reason }) => { client.shutdown().await; return Err(AgentError::Network(reason)); },
                    Ok(AcpEvent::Request { .. }) => {
                        client.shutdown().await;
                        return Err(AgentError::NotAvailable("ACP requested user interaction before the session was loaded; open it in the agent's terminal".into()));
                    },
                    Ok(_) => {},
                    Err(_) => { client.shutdown().await; return Err(protocol("ACP history stream was interrupted")); },
                },
                value = &mut response => break value,
            }
        };
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                client.shutdown().await;
                return Err(AgentError::RequestFailed(error));
            }
        };
        while let Ok(event) = events.try_recv() {
            match event {
                AcpEvent::Notification { method, params } if method == "session/update" => {
                    replay_bytes = replay_bytes.saturating_add(
                        serde_json::to_vec(&params)
                            .map(|bytes| bytes.len())
                            .unwrap_or(usize::MAX),
                    );
                    if replay_bytes > 32 * 1024 * 1024 || replay.len() >= 100_000 {
                        client.shutdown().await;
                        return Err(AgentError::NotAvailable(
                            "ACP whole-history replay exceeds the Gateway budget".into(),
                        ));
                    }
                    replay.push(params);
                }
                AcpEvent::Disconnected { reason } => {
                    client.shutdown().await;
                    return Err(AgentError::Network(reason));
                }
                AcpEvent::Request { .. } => {
                    client.shutdown().await;
                    return Err(AgentError::NotAvailable("ACP requested interaction before session load completed; use the agent's terminal".into()));
                }
                _ => {}
            }
        }
        let id = id.or_else(|| value["sessionId"].as_str().map(str::to_owned)).ok_or_else(|| protocol("ACP session creation returned no sessionId; do not retry creation automatically"))?;
        let mut info = existing.unwrap_or_else(|| mapper::session(&id, directory, &value));
        info.status = AgentSessionStatus::Idle;
        info.model = mapper::selected_model_for(&value, P::KIND);
        if info.updated_ms == 0 {
            info.updated_ms = mapper::now_ms();
        }
        info.mode = value
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (completions, completed) = mpsc::unbounded_channel();
        let mut state = SessionState {
            info,
            catalog: mapper::catalog_for(&value, P::KIND),
            config: value.clone(),
            projection: Projection::new(P::REQUIRE_MESSAGE_ID),
            permissions: HashMap::new(),
        };
        for params in replay {
            if params["sessionId"].as_str() != Some(id.as_str()) {
                client.shutdown().await;
                return Err(protocol("ACP history replay changed session identity"));
            }
            if let Err(error) = Self::reduce(&mut state, &params["update"]) {
                client.shutdown().await;
                return Err(error);
            }
        }
        if self.stop.is_cancelled() {
            client.shutdown().await;
            return Err(AgentError::NotAvailable(
                "ACP runtime stopped while loading a session".into(),
            ));
        }
        // Native replay ids need not survive a load. Only a fully validated
        // replacement may replace the old snapshot; no replay row is broadcast.
        let snapshot = AgentSessionSnapshot {
            info: state.info.clone(),
            timeline: state
                .projection
                .timeline()
                .into_iter()
                .map(|mut row| {
                    row.seq = 0;
                    row
                })
                .collect(),
            permissions: Vec::new(),
            forms: Vec::new(),
            inbox: Vec::new(),
            seq: 0,
        };
        if self
            .mirror
            .refresh_prepared_snapshot(snapshot, revision.0, revision.1, None)
            .await?
            .is_none()
        {
            client.shutdown().await;
            return Err(AgentError::NotAvailable(
                "ACP session changed while loading; refresh again".into(),
            ));
        }
        let session = Arc::new(Session {
            client,
            state: Mutex::new(state),
            busy: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            completions,
            finished: CancellationToken::new(),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());
        Self::emit(
            &self.mirror,
            &self.tx,
            AgentDomainEvent::Resync {
                asid: id.as_str().into(),
                reason: "acp_session_loaded".into(),
            },
        );
        Self::listen(
            session.clone(),
            events,
            completed,
            self.mirror.clone(),
            self.tx.clone(),
            self.stop.child_token(),
        );
        Ok(session)
    }

    fn emit(
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        mut event: AgentDomainEvent,
    ) {
        mirror.stamp_event(&mut event);
        let _ = tx.send(event);
    }

    async fn status(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        status: AgentSessionStatus,
        error: Option<AgentErrorInfo>,
    ) {
        let asid = {
            let mut state = session.state.lock().await;
            state.info.status = status;
            state.info.error = error.clone();
            state.info.updated_ms = mapper::now_ms();
            state.info.asid.clone()
        };
        if let Some(seq) = mirror.update_status(&asid, status, error.clone()).await {
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::StatusChanged {
                    asid,
                    status,
                    error,
                    seq,
                },
            );
        }
    }

    fn reduce(
        state: &mut SessionState,
        update: &Value,
    ) -> Result<Option<TimelineItem>, AgentError> {
        match update["sessionUpdate"].as_str() {
            Some("session_info_update") => {
                if let Some(title) = update["title"].as_str() {
                    state.info.title = title.into();
                }
                Ok(None)
            }
            Some("usage_update") => {
                if let Some(used) = update["used"].as_u64() {
                    state.info.context_usage = Some(SessionContextUsage {
                        tokens: None,
                        total_tokens: used,
                        limit: update["size"].as_u64().map(|size| json!({"context":size})),
                    });
                }
                Ok(None)
            }
            Some("current_mode_update") => {
                state.info.mode = update["currentModeId"].as_str().map(str::to_owned);
                Ok(None)
            }
            Some("config_option_update") => {
                let mut catalog = mapper::catalog_for(update, P::KIND);
                catalog.modes = state.catalog.modes.clone();
                catalog.defaults.mode = state.catalog.defaults.mode.clone();
                state.catalog = catalog;
                state.config["configOptions"] = update["configOptions"].clone();
                state.info.model = mapper::selected_model_for(update, P::KIND);
                Ok(None)
            }
            _ => state.projection.update(update),
        }
    }

    async fn notification(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        params: &Value,
    ) {
        let mut state = session.state.lock().await;
        let asid = state.info.asid.clone();
        if params["sessionId"].as_str() != Some(asid.0.as_str()) {
            session.connected.store(false, Ordering::Release);
            state.info.status = AgentSessionStatus::Unknown;
            if let Some(seq) = mirror
                .update_status(&asid, AgentSessionStatus::Unknown, None)
                .await
            {
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::StatusChanged {
                        asid: asid.clone(),
                        status: AgentSessionStatus::Unknown,
                        error: None,
                        seq,
                    },
                );
            }
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::Resync {
                    asid,
                    reason: "ACP event changed session identity".into(),
                },
            );
            return;
        }
        let old_info = state.info.clone();
        match Self::reduce(&mut state, &params["update"]) {
            Ok(Some(row)) => {
                let event = mirror.record_timeline_upsert(&asid, vec![row]).await;
                Self::emit(mirror, tx, event);
            }
            Ok(None) if old_info != state.info => {
                let info = state.info.clone();
                let seq = mirror.update_session(info.clone()).await;
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::SessionUpdated {
                        asid,
                        info: Box::new(info),
                        seq,
                    },
                );
            }
            Ok(None) => {}
            Err(error) => {
                session.connected.store(false, Ordering::Release);
                state.info.status = AgentSessionStatus::Unknown;
                let error_info = Some(AgentErrorInfo {
                    name: "AcpHistoryUnavailable".into(),
                    message: error.to_string(),
                    status: None,
                    retry_after_ms: None,
                });
                state.info.error = error_info.clone();
                if let Some(seq) = mirror
                    .update_status(&asid, AgentSessionStatus::Unknown, error_info.clone())
                    .await
                {
                    Self::emit(
                        mirror,
                        tx,
                        AgentDomainEvent::StatusChanged {
                            asid: asid.clone(),
                            status: AgentSessionStatus::Unknown,
                            error: error_info,
                            seq,
                        },
                    );
                }
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::Resync {
                        asid,
                        reason: error.to_string(),
                    },
                );
            }
        }
    }

    async fn event(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        event: AcpEvent,
    ) -> bool {
        match event {
            AcpEvent::Notification { method, params } if method == "session/update" => {
                Self::notification(session, mirror, tx, &params).await
            }
            AcpEvent::Request { id, method, params } if method == "session/request_permission" => {
                match mapper::permission(id.clone(), &params) {
                    Ok(mut permission) => {
                        let mut state = session.state.lock().await;
                        if permission.request.asid != state.info.asid {
                            let _ = session
                                .client
                                .respond_error(id, -32602, "Session identity mismatch")
                                .await;
                            return true;
                        }
                        let mut tool = params["toolCall"].clone();
                        tool["sessionUpdate"] = json!("tool_call");
                        match state.projection.update(&tool) {
                            Ok(Some(row)) => {
                                permission.request.source_message_id = Some(row.message_id.clone());
                                let event = mirror
                                    .record_timeline_upsert(&state.info.asid, vec![row])
                                    .await;
                                Self::emit(mirror, tx, event);
                            }
                            Ok(None) => {}
                            Err(error) => {
                                session.connected.store(false, Ordering::Release);
                                state.info.status = AgentSessionStatus::Unknown;
                                let asid = state.info.asid.clone();
                                if let Some(seq) = mirror
                                    .update_status(&asid, AgentSessionStatus::Unknown, None)
                                    .await
                                {
                                    Self::emit(
                                        mirror,
                                        tx,
                                        AgentDomainEvent::StatusChanged {
                                            asid: asid.clone(),
                                            status: AgentSessionStatus::Unknown,
                                            error: None,
                                            seq,
                                        },
                                    );
                                }
                                Self::emit(
                                    mirror,
                                    tx,
                                    AgentDomainEvent::Resync {
                                        asid,
                                        reason: error.to_string(),
                                    },
                                );
                                return false;
                            }
                        }
                        let request = permission.request.clone();
                        state.permissions.insert(request.id.clone(), permission);
                        let seq = mirror.add_permission(request.clone()).await;
                        Self::emit(
                            mirror,
                            tx,
                            AgentDomainEvent::PermissionPending {
                                asid: request.asid.clone(),
                                request,
                                seq,
                            },
                        );
                    }
                    Err(_) => {
                        let _ = session
                            .client
                            .respond_error(id, -32602, "Invalid permission request")
                            .await;
                    }
                }
            }
            AcpEvent::Disconnected { reason } => {
                session.connected.store(false, Ordering::Release);
                Self::status(
                    session,
                    mirror,
                    tx,
                    AgentSessionStatus::Unknown,
                    Some(AgentErrorInfo {
                        name: "AcpDisconnected".into(),
                        message: format!("{reason}; prompt delivery will not be retried"),
                        status: None,
                        retry_after_ms: None,
                    }),
                )
                .await;
                Self::clear_permissions(session, mirror, tx).await;
                return false;
            }
            _ => {}
        }
        session.connected.load(Ordering::Acquire)
    }

    async fn clear_permissions(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
    ) {
        let mut state = session.state.lock().await;
        let asid = state.info.asid.clone();
        for (request_id, _) in state.permissions.drain() {
            let seq = mirror.resolve_permission(&asid, &request_id).await;
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::PermissionResolved {
                    asid: asid.clone(),
                    request_id,
                    seq,
                },
            );
        }
    }

    fn listen(
        session: Arc<Session>,
        mut events: broadcast::Receiver<AcpEvent>,
        mut completed: mpsc::UnboundedReceiver<Result<Value, String>>,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
        stop: CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(event) => if !Self::event(&session, &mirror, &tx, event).await { break; },
                        Err(_) => { Self::status(&session, &mirror, &tx, AgentSessionStatus::Unknown, None).await; break; }
                    },
                    Some(result) = completed.recv() => {
                        // The RPC reader queued all updates before resolving this reply.
                        while let Ok(event) = events.try_recv() { if !Self::event(&session, &mirror, &tx, event).await { break; } }
                        if !session.connected.load(Ordering::Acquire) { break; }
                        let (status, error) = match result {
                            Ok(value) => match value["stopReason"].as_str() {
                                Some("cancelled") => (AgentSessionStatus::Interrupted, None),
                                Some("end_turn" | "max_tokens" | "max_turn_requests" | "refusal") => (AgentSessionStatus::Idle, None),
                                _ => (AgentSessionStatus::Unknown, Some(AgentErrorInfo { name: "AcpUnknownStop".into(), message: "ACP returned no recognized stop reason".into(), status: None, retry_after_ms: None })),
                            },
                            Err(error) => (AgentSessionStatus::Failed, Some(AgentErrorInfo { name: "AcpPromptFailed".into(), message: format!("{error}; delivery was not retried"), status: None, retry_after_ms: None })),
                        };
                        Self::clear_permissions(&session, &mirror, &tx).await;
                        Self::status(&session, &mirror, &tx, status, error).await;
                        session.busy.store(false, Ordering::Release);
                    }
                }
            }
            session.connected.store(false, Ordering::Release);
            Self::clear_permissions(&session, &mirror, &tx).await;
            session.client.shutdown().await;
            session.finished.cancel();
        });
    }
}

impl<P: AcpProvider> Drop for AcpDriver<P> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl<P: AcpProvider> AgentPort for AcpDriver<P> {
    fn kind(&self) -> &'static str {
        P::KIND
    }
    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async { Ok(self.is_connected()) })
    }
    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::ByPath
    }
    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let mut directories = HashSet::new();
            Ok(self
                .native_sessions(None)
                .await?
                .into_iter()
                .filter_map(|info| info.directory)
                .filter(|directory| directories.insert(directory.clone()))
                .map(|directory| AgentProject {
                    id: directory.clone(),
                    canonical: directory.clone(),
                    name: std::path::Path::new(&directory)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| directory.clone()),
                    vcs: None,
                    sandboxes: Vec::new(),
                    missing: !std::path::Path::new(&directory).is_dir(),
                })
                .collect())
        })
    }
    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            if query.cursor.is_some() {
                return Err(AgentError::Unsupported("session pagination".into()));
            }
            let mut sessions = self.native_sessions(query.directory.as_deref()).await?;
            sessions.retain(|info| {
                query.keeps_parent(info.parent_id.as_deref())
                    && query.search.as_ref().is_none_or(|search| {
                        info.title.to_lowercase().contains(&search.to_lowercase())
                    })
            });
            sessions.sort_by(|a, b| {
                b.updated_ms
                    .cmp(&a.updated_ms)
                    .then_with(|| a.asid.0.cmp(&b.asid.0))
            });
            if query.order.as_deref() == Some("asc") {
                sessions.reverse();
            }
            if let Some(limit) = query.limit {
                sessions.truncate(limit);
            }
            Ok(sessions)
        })
    }
    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        mode: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if model.is_some() && !P::MODEL_SELECTION {
                return Err(AgentError::Unsupported("ACP model selection".into()));
            }
            if mode.is_some() {
                return Err(AgentError::Unsupported("ACP mode selection".into()));
            }
            let directory = directory.unwrap_or(&self.default_directory);
            if !std::path::Path::new(directory).is_dir() {
                return Err(AgentError::WorkspaceMissing(directory.into()));
            }
            let _mutation = self.mutations.lock().await;
            let session = self.open(directory, None).await?;
            if let Some(model) = model {
                if let Err(error) = self.select_model(&session, model).await {
                    let id = session.state.lock().await.info.asid.0.clone();
                    return Err(AgentError::RequestFailed(format!("ACP session {id} was created, but model selection failed: {error}; do not retry creation")));
                }
            }
            let info = session.state.lock().await.info.clone();
            Ok(info)
        })
    }
    fn get_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if let Some(session) = self.retained(id) {
                return Ok(session.state.lock().await.info.clone());
            }
            self.native_sessions(None)
                .await?
                .into_iter()
                .find(|info| info.asid.0 == id)
                .ok_or_else(|| AgentError::SessionNotFound(id.into()))
        })
    }
    fn prepare_selected_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // Only an explicit selection/refresh may replace a dead connection.
            // Wait for the old reducer to stop before a same-id load publishes.
            {
                let _mutation = self.mutations.lock().await;
                if let Some(session) = self.retained(id) {
                    if !session.connected.load(Ordering::Acquire) {
                        session.client.shutdown().await;
                        tokio::time::timeout(Duration::from_secs(5), session.finished.cancelled())
                            .await
                            .map_err(|_| {
                                AgentError::NotAvailable(
                                    "ACP previous connection has not stopped yet".into(),
                                )
                            })?;
                        self.sessions.lock().unwrap().remove(id);
                    }
                }
            }
            self.ensure_loaded(id).await.map(|_| ())
        })
    }
    fn send_prompt<'a>(
        &'a self,
        id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if delivery.is_some_and(|delivery| delivery != "auto") {
                return Err(AgentError::Unsupported(
                    "ACP prompt steering or queueing".into(),
                ));
            }
            if !attachments.is_empty() {
                return Err(AgentError::InvalidRequest(
                    "ACP attachments must pass through the Gateway upload use case".into(),
                ));
            }
            let session = self.ensure_loaded(id).await?;
            if session
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(AgentError::NotAvailable(
                    "ACP session is already processing a turn".into(),
                ));
            }
            let mut admission = Admission {
                busy: &session.busy,
                handed_off: false,
            };
            // Admission and the local user row share the state lock: a fast
            // native response cannot appear ahead of its submitted user text.
            // Busy is published only after a request was actually flushed.
            let mut state = session.state.lock().await;
            state.projection.can_submit(text)?;
            let request = session
                .client
                .start_request(
                    "session/prompt",
                    json!({ "sessionId": id, "prompt": [{ "type": "text", "text": text }] }),
                )
                .await;
            match request {
                Ok(request) => {
                    state.info.status = AgentSessionStatus::Busy;
                    state.info.error = None;
                    state.info.updated_ms = mapper::now_ms();
                    if let Some(seq) = self
                        .mirror
                        .update_status(&state.info.asid, AgentSessionStatus::Busy, None)
                        .await
                    {
                        Self::emit(
                            &self.mirror,
                            &self.tx,
                            AgentDomainEvent::StatusChanged {
                                asid: state.info.asid.clone(),
                                status: AgentSessionStatus::Busy,
                                error: None,
                                seq,
                            },
                        );
                    }
                    let row = state.projection.submitted(text);
                    let event = self
                        .mirror
                        .record_timeline_upsert(&state.info.asid, vec![row])
                        .await;
                    Self::emit(&self.mirror, &self.tx, event);
                    drop(state);
                    let sender = session.completions.clone();
                    tokio::spawn(async move {
                        let _ = sender.send(request.wait().await);
                    });
                    admission.handed_off = true;
                    Ok(())
                }
                Err(error) => {
                    drop(state);
                    // A partial stdio write can be ambiguous. Preserve the uncertainty;
                    // no automatic retry, queue fallback or second delivery is allowed.
                    session.connected.store(false, Ordering::Release);
                    Self::status(
                        &session,
                        &self.mirror,
                        &self.tx,
                        AgentSessionStatus::Unknown,
                        None,
                    )
                    .await;
                    Err(AgentError::Network(format!(
                        "{error}; prompt delivery may have occurred; do not retry automatically"
                    )))
                }
            }
        })
    }
    fn interrupt<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let session = self
                .retained(id)
                .ok_or_else(|| AgentError::SessionNotFound(id.into()))?;
            if !session.connected.load(Ordering::Acquire) {
                return Err(AgentError::NotAvailable("ACP connection ended".into()));
            }
            session
                .client
                .notify("session/cancel", json!({"sessionId":id}))
                .await
                .map_err(AgentError::Network)
        })
    }
    fn reply_permission<'a>(
        &'a self,
        id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if message.is_some_and(|message| !message.is_empty()) {
                return Err(AgentError::Unsupported(
                    "ACP permission rejection reasons".into(),
                ));
            }
            let session = self
                .retained(id)
                .ok_or_else(|| AgentError::SessionNotFound(id.into()))?;
            let mut state = session.state.lock().await;
            let permission = state.permissions.get(request_id).ok_or_else(|| {
                AgentError::InvalidRequest("ACP permission is no longer pending".into())
            })?;
            let option = permission
                .options
                .iter()
                .filter(|(candidate, _)| *candidate == decision)
                .collect::<Vec<_>>();
            if option.len() != 1 {
                return Err(AgentError::Unsupported(
                    "requested ACP permission option is unavailable or ambiguous".into(),
                ));
            }
            session
                .client
                .respond(
                    permission.rpc_id.clone(),
                    json!({"outcome":{"outcome":"selected","optionId":option[0].1}}),
                )
                .await
                .map_err(AgentError::Network)?;
            state.permissions.remove(request_id);
            let asid = AgentSessionId::from(id);
            let seq = self.mirror.resolve_permission(&asid, request_id).await;
            Self::emit(
                &self.mirror,
                &self.tx,
                AgentDomainEvent::PermissionResolved {
                    asid,
                    request_id: request_id.into(),
                    seq,
                },
            );
            Ok(())
        })
    }
    fn get_pending_permissions<'a>(
        &'a self,
        id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            let session = self.ensure_loaded(id).await?;
            let pending = session
                .state
                .lock()
                .await
                .permissions
                .values()
                .map(|permission| permission.request.clone())
                .collect();
            Ok(pending)
        })
    }
    fn get_pending_forms<'a>(&'a self, id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async move {
            self.get_session(id).await?;
            Ok(Vec::new())
        })
    }
    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async { Ok(self.observed_catalog().await) })
    }
    fn get_catalog_for_session<'a>(
        &'a self,
        id: Option<&'a str>,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            if let Some(id) = id {
                let session = self.ensure_loaded(id).await?;
                let mut catalog = session.state.lock().await.catalog.clone();
                // ACP mode changes persist permission defaults outside the current
                // session, so the existing App must not offer them as local choices.
                catalog.modes.clear();
                catalog.defaults.mode = None;
                Ok(catalog)
            } else {
                self.get_catalog(directory).await
            }
        })
    }
    fn get_timeline<'a>(
        &'a self,
        id: &'a str,
        _limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let session = self.ensure_loaded(id).await?;
            let timeline = session
                .state
                .lock()
                .await
                .projection
                .timeline()
                .into_iter()
                .map(|mut row| {
                    row.seq = 0;
                    row
                })
                .collect();
            Ok(timeline)
        })
    }
    fn revert_session<'a>(&'a self, _id: &'a str, _message: &'a str) -> AgentFuture<'a, ()> {
        unsupported("revert")
    }
    fn switch_model<'a>(&'a self, id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let session = self.ensure_loaded(id).await?;
            self.select_model(&session, model).await
        })
    }
    fn switch_mode<'a>(&'a self, _id: &'a str, _mode: &'a str) -> AgentFuture<'a, ()> {
        unsupported("mode selection")
    }
    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        unsupported("file search")
    }
    fn reply_form<'a>(
        &'a self,
        _id: &'a str,
        _form: &'a str,
        _answers: Value,
    ) -> AgentFuture<'a, ()> {
        unsupported("interactive forms")
    }
    fn get_vcs_diff<'a>(
        &'a self,
        _id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        unsupported("VCS diff")
    }
}

#[cfg(all(test, unix))]
mod admission_tests {
    use super::*;
    use crate::agents::adapters::fx::tests::integration::Fixture;

    #[tokio::test]
    async fn cancelled_before_dispatch_keeps_idle_and_sends_no_prompt() {
        let fixture = Fixture::new().await;
        let info = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let session = fixture.driver.retained(&info.asid.0).unwrap();
        let projection = session.state.lock().await;
        let driver = fixture.driver.clone();
        let id = info.asid.0.clone();
        let pending =
            tokio::spawn(
                async move { driver.send_prompt(&id, "never dispatched", &[], None).await },
            );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.busy.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(!session.busy.load(Ordering::Acquire));
        assert_eq!(projection.info.status, AgentSessionStatus::Idle);
        drop(projection);
        let snapshot = fixture
            .driver
            .mirror
            .get_snapshot(&info.asid)
            .await
            .unwrap();
        assert_eq!(snapshot.info.status, AgentSessionStatus::Idle);
        assert!(snapshot.timeline.is_empty());
        assert!(!fixture.methods().contains(&"session/prompt".into()));
    }
}
