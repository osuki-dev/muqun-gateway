use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{
    budget::{next_cursor, Budget},
    mapper, CodexClient,
};
use crate::agents::domain::*;
use crate::agents::ports::agent::{
    append_attachment_paths, AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem,
};

const MAX_SESSIONS: usize = 4096;
const MAX_HISTORY_ITEMS: usize = 2000;
// Leave room to insert an older authoritative prefix before a live/tail anchor.
const FIRST_ITEM_POSITION: u64 = 1 << 63;

pub struct CodexDriver {
    client: Arc<CodexClient>,
    terminal_runtime: Option<Arc<crate::agents::terminal_runtime::TerminalRuntime>>,
    /// Only explicitly created, forked or submitted-to threads are resumed.
    loaded: Mutex<HashSet<String>>,
    /// Serializes explicit mutations, not the stream or read-only RPC calls.
    mutations: Arc<Mutex<()>>,
    order: std::sync::Mutex<HashMap<(String, String, String), u64>>,
    next_models: Arc<std::sync::Mutex<HashMap<String, PendingModel>>>,
    metadata_versions: Arc<Mutex<HashMap<String, u64>>>,
    next_metadata_version: std::sync::atomic::AtomicU64,
    refresh_slots: Arc<tokio::sync::Semaphore>,
    usage: Mutex<HashMap<String, super::usage::Usage>>,
    pub(crate) rollouts: super::rollout::Rollouts,
}

struct PendingModel {
    model: ModelRef,
    accepted_revision: Option<u64>,
}

fn unsupported<T>(capability: &str) -> Result<T, AgentError> {
    Err(AgentError::Unsupported(capability.into()))
}

fn validate_id(id: &str) -> Result<(), AgentError> {
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(AgentError::InvalidRequest("invalid Codex thread id".into()));
    }
    Ok(())
}

impl CodexDriver {
    pub fn new(client: Arc<CodexClient>) -> Self {
        Self {
            client,
            terminal_runtime: None,
            loaded: Mutex::new(HashSet::new()),
            mutations: Arc::new(Mutex::new(())),
            order: std::sync::Mutex::new(HashMap::new()),
            next_models: Arc::new(std::sync::Mutex::new(HashMap::new())),
            metadata_versions: Arc::new(Mutex::new(HashMap::new())),
            next_metadata_version: std::sync::atomic::AtomicU64::new(1),
            refresh_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            usage: Mutex::new(HashMap::new()),
            rollouts: Default::default(),
        }
    }

    pub(crate) fn observe_terminals(
        &mut self,
        observer: Option<Arc<crate::agents::terminal_runtime::TerminalRuntime>>,
    ) {
        self.terminal_runtime = observer;
    }

    pub(crate) async fn runtime(&self, id: &str, fresh: bool) -> Option<SessionRuntime> {
        // Same-server accepted rejoin is stronger than a terminal observation:
        // both clients control this one runtime. Private writers keep the old
        // conservative terminal precedence.
        if self.client.is_shared() && self.client.is_subscribed(id) {
            return Some(SessionRuntime {
                transcript: Some(TranscriptSource::Live),
                source: RuntimeSource::Native,
                interaction: RuntimeInteraction::Native,
                status: None,
                terminal: None,
                reason: None,
            });
        }
        let observer = self.terminal_runtime.as_ref()?;
        let native = self.loaded.lock().await.contains(id) && !self.client.is_shared();
        Some(
            self.rollouts
                .decorate(id, observer.observe(id, native, fresh).await),
        )
    }

    async fn prepare_shared_interaction(&self, id: &str) -> Result<(), AgentError> {
        if self.client.is_shared() && !self.client.is_subscribed(id) {
            let thread = self.read_thread(id).await?;
            self.rejoin_loaded(id, &thread).await?;
        }
        Ok(())
    }

    /// Local metadata only. Native event processing must not await Herdr I/O.
    pub(crate) async fn cached_runtime(&self, id: &str) -> Option<SessionRuntime> {
        if self.client.is_shared() && self.client.is_subscribed(id) {
            return Some(SessionRuntime {
                transcript: Some(TranscriptSource::Live),
                source: RuntimeSource::Native,
                interaction: RuntimeInteraction::Native,
                status: None,
                terminal: None,
                reason: None,
            });
        }
        let observer = self.terminal_runtime.as_ref()?;
        let native = self.loaded.lock().await.contains(id) && !self.client.is_shared();
        Some(self.rollouts.decorate(id, observer.cached(id, native)))
    }

    async fn require_native_interaction(&self, id: &str) -> Result<(), AgentError> {
        if let Some(runtime) = self.runtime(id, true).await {
            if runtime.interaction != RuntimeInteraction::Native {
                return Err(AgentError::TerminalInteraction(Box::new(runtime)));
            }
        }
        Ok(())
    }

    pub fn client(&self) -> &Arc<CodexClient> {
        &self.client
    }

    pub(crate) async fn record_usage(&self, id: &str, value: &Value) -> Result<(), AgentError> {
        validate_id(id)?;
        let mut usage = self.usage.lock().await;
        if usage.len() >= MAX_SESSIONS && !usage.contains_key(id) {
            self.client.stop();
            return Err(AgentError::NotAvailable(
                "Codex usage capacity exhausted".into(),
            ));
        }
        usage.insert(id.into(), super::usage::Usage::from_wire(value));
        Ok(())
    }

    pub(crate) async fn clear_usage(&self, id: &str) {
        self.usage.lock().await.remove(id);
    }

    pub(crate) async fn cached_usage(&self, id: &str) -> Option<super::usage::Usage> {
        self.usage.lock().await.get(id).cloned()
    }

    pub(crate) async fn apply_usage(&self, info: &mut AgentSessionInfo) {
        if let Some(usage) = self.usage.lock().await.get(&info.asid.0) {
            usage.apply(info);
        }
    }

    // Held across the manager's mirror update/send to linearize publication
    // against a newer refresh or accepted turn, not just check before an await.
    pub(crate) async fn metadata_guard(&self) -> tokio::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.metadata_versions.lock().await
    }

    /// Reads are versioned when scheduled, not on completion. A later turn/profile
    /// refresh fences older reads even when their native replies arrive last.
    async fn metadata_revision(&self, id: &str) -> Result<u64, AgentError> {
        validate_id(id)?;
        let revision = {
            let mut versions = self.metadata_versions.lock().await;
            if versions.len() >= MAX_SESSIONS && !versions.contains_key(id) {
                self.client.stop();
                return Err(AgentError::NotAvailable(
                    "Codex metadata capacity exhausted; resync required".into(),
                ));
            }
            let revision = self
                .next_metadata_version
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            versions.insert(id.into(), revision);
            revision
        };
        Ok(revision)
    }

    /// The task owns only bounded RPC state, never the manager/event pump.
    /// Client cancellation terminates its read on shutdown/replacement. Caller
    /// cancellation after acceptance cannot cancel metadata reconciliation.
    fn spawn_metadata_refresh(&self, id: &str, revision: u64) {
        // Bound workers waiting at the mutation boundary as well as RPCs that
        // have reached the client's own pending-request capacity check.
        let Ok(permit) = self.refresh_slots.clone().try_acquire_owned() else {
            self.client.stop();
            return;
        };
        let id = id.to_owned();
        let client = self.client.clone();
        let versions = self.metadata_versions.clone();
        let choices = self.next_models.clone();
        let mutations = self.mutations.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // Never dispatch a rename/placeholder read in the middle of an
            // outstanding turn/start: it could return the pre-accept selection
            // with a newer revision. Only wait for the mutation boundary; reads
            // remain concurrent and a subsequent mutation fences their results.
            let cancel = client.cancel_token();
            let guard = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                guard = mutations.lock() => guard,
            };
            drop(guard);
            if versions.lock().await.get(&id).copied() != Some(revision) {
                return;
            }
            let result = async {
                let response = client
                    .call("thread/read", json!({"threadId":id,"includeTurns":false}))
                    .await?;
                let thread = response["thread"].clone();
                if thread["id"] != id {
                    return Err(AgentError::Protocol(
                        "Codex thread/read identity mismatch".into(),
                    ));
                }
                mapper::session(&thread)?;
                Ok(thread)
            }
            .await;
            let versions = versions.lock().await;
            if versions.get(&id).copied() != Some(revision) || !client.is_ready() {
                return;
            }
            match result {
                Ok(thread) => {
                    let mut choices = choices.lock().unwrap_or_else(|p| p.into_inner());
                    if choices
                        .get(&id)
                        .and_then(|choice| choice.accepted_revision)
                        .is_some_and(|accepted| accepted <= revision)
                    {
                        choices.remove(&id);
                    }
                    let _ = client.snapshot(thread, revision);
                }
                Err(_) => {
                    let _ = client.metadata_refresh_failed(&id, revision);
                }
            }
        });
    }

    pub(crate) async fn refresh_metadata(&self, id: &str) -> Result<(), AgentError> {
        let revision = self.metadata_revision(id).await?;
        self.spawn_metadata_refresh(id, revision);
        Ok(())
    }

    /// Item ids are opaque, unlike UUIDv7 turn ids. Order comes from the
    /// producer's item array or first live observation, never lexical item ids
    /// or a fabricated upstream replay sequence. Row ids remain independent.
    pub(crate) fn item_position(
        &self,
        thread: &str,
        turn: &str,
        item: &str,
    ) -> Result<u64, AgentError> {
        let mut order = self.order.lock().unwrap_or_else(|p| p.into_inner());
        let key = (thread.to_string(), turn.to_string(), item.to_string());
        if order.len() >= 8192 && !order.contains_key(&key) {
            self.client.stop();
            return Err(AgentError::NotAvailable(
                "Codex item-order capacity exhausted; resync required".into(),
            ));
        }
        let position = order.get(&key).copied().unwrap_or_else(|| {
            order
                .iter()
                .filter(|((t, u, _), _)| t == thread && u == turn)
                .map(|(_, position)| position.saturating_add(1))
                .max()
                .unwrap_or(FIRST_ITEM_POSITION)
        });
        order.insert(key, position);
        Ok(position)
    }

    fn set_history_order(
        &self,
        thread: &str,
        turn: &str,
        ids: &[String],
    ) -> Result<(), AgentError> {
        let mut order = self.order.lock().unwrap_or_else(|p| p.into_inner());
        let key = |id: &str| (thread.to_string(), turn.to_string(), id.to_string());
        let new_ids: HashSet<_> = ids
            .iter()
            .filter(|id| !order.contains_key(&key(id)))
            .collect();
        if order.len() + new_ids.len() > 8192 {
            self.client.stop();
            return Err(AgentError::NotAvailable(
                "Codex item-order capacity exhausted; resync required".into(),
            ));
        }
        // Preserve a shared anchor when a larger or newer tail is requested.
        // Provider array order is authoritative even when Unix clocks regress.
        let base = ids
            .iter()
            .enumerate()
            .find_map(|(index, id)| {
                order
                    .get(&key(id))
                    .and_then(|p| p.checked_sub(index as u64))
            })
            .unwrap_or_else(|| {
                order
                    .iter()
                    .filter(|((t, u, _), _)| t == thread && u == turn)
                    .map(|(_, p)| p.saturating_add(1))
                    .max()
                    .unwrap_or(FIRST_ITEM_POSITION)
            });
        for (index, id) in ids.iter().enumerate() {
            order.insert(key(id), base.saturating_add(index as u64));
        }
        Ok(())
    }

    pub(crate) fn ordered_items(
        &self,
        thread: &str,
        turn: &str,
        item: &Value,
        at_ms: u64,
    ) -> Result<Vec<TimelineItem>, AgentError> {
        if item["type"] == "reasoning"
            && ["summary", "content"].iter().any(|key| {
                item[*key]
                    .as_array()
                    .is_some_and(|parts| parts.len() > super::MAX_REASONING_PARTS)
            })
        {
            self.client.stop();
            return Err(AgentError::Protocol(
                "Codex reasoning part limit exceeded; resync required".into(),
            ));
        }
        if item["receiverThreadIds"].as_array().is_some_and(|targets| {
            targets.len() > 512
                || serde_json::to_vec(item).map_or(true, |wire| {
                    wire.len().saturating_mul(targets.len().max(1)) > super::client::MAX_FRAME_BYTES
                })
        }) {
            self.client.stop();
            return Err(AgentError::Protocol(
                "Codex collaboration target limit exceeded; resync required".into(),
            ));
        }
        let id = mapper::string(item, "id")?;
        let position = self.item_position(thread, turn, &id)?;
        let mut rows = mapper::item(thread, turn, item, at_ms);
        for row in &mut rows {
            row.message_id = mapper::order_group(thread, turn, position);
        }
        Ok(rows)
    }

    pub(crate) fn permission(
        &self,
        id: &Value,
        method: &str,
        params: &Value,
    ) -> Option<PermissionRequest> {
        let mut request = mapper::permission(id, method, params)?;
        request.id = self.client.request_id(id)?;
        let position = self
            .item_position(
                params["threadId"].as_str()?,
                params["turnId"].as_str()?,
                params["itemId"].as_str()?,
            )
            .ok()?;
        request.source_message_id = Some(mapper::order_group(
            params["threadId"].as_str()?,
            params["turnId"].as_str()?,
            position,
        ));
        Some(request)
    }

    pub(crate) fn form(&self, id: &Value, method: &str, params: &Value) -> Option<FormRequest> {
        let mut request = mapper::form(id, method, params)?;
        request.id = self.client.request_id(id)?;
        Some(request)
    }

    async fn remember(&self, id: &str) -> Result<(), AgentError> {
        let mut loaded = self.loaded.lock().await;
        if loaded.len() >= MAX_SESSIONS && !loaded.contains(id) {
            return Err(AgentError::NotAvailable(
                "Codex loaded-thread capacity exhausted".into(),
            ));
        }
        loaded.insert(id.into());
        Ok(())
    }

    pub(crate) async fn forget_thread(&self, id: &str) {
        self.loaded.lock().await.remove(id);
        self.next_models
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        self.metadata_versions.lock().await.remove(id);
        self.order
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(thread, _, _), _| thread != id);
    }

    /// Opening a requested timeline may rejoin a runtime already loaded in
    /// the shared daemon. Metadata/history-only external threads stay untouched.
    async fn rejoin_loaded(&self, id: &str, metadata: &Value) -> Result<(), AgentError> {
        if !self.client.is_shared()
            || self.client.is_subscribed(id)
            || !matches!(metadata["status"]["type"].as_str(), Some("idle" | "active"))
        {
            return Ok(());
        }
        let _guard = self.mutations.lock().await;
        if self.client.is_subscribed(id) {
            return Ok(());
        }
        // Recheck after waiting for another mutation; never turn a known
        // notLoaded external CLI thread into a resume/takeover attempt.
        let fresh = self.read_thread(id).await?;
        if !matches!(fresh["status"]["type"].as_str(), Some("idle" | "active")) {
            return Ok(());
        }
        let resumed = self
            .client
            .call("thread/resume", json!({"threadId":id,"excludeTurns":true}))
            .await?;
        if resumed["thread"]["id"] != id {
            self.client.stop();
            return Err(AgentError::Protocol(
                "Codex rejoin identity mismatch".into(),
            ));
        }
        self.selected_response(resumed).await?;
        // Metadata-only rejoin does not repeat turn/started for an existing
        // turn. Read its authoritative id without hydrating unbounded history.
        let page = self
            .client
            .call(
                "thread/turns/list",
                json!({
                    "threadId":id,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"
                }),
            )
            .await?;
        let turns = page["data"]
            .as_array()
            .filter(|rows| rows.len() <= 1)
            .ok_or_else(|| AgentError::Protocol("invalid Codex active turn page".into()))?;
        for turn in turns {
            self.client.observe_active_turn(id, turn);
        }
        Ok(())
    }

    async fn selected_response(&self, response: Value) -> Result<AgentSessionInfo, AgentError> {
        let mut thread = response["thread"].clone();
        // The response, never the requested model, is the source of truth.
        for key in ["model", "modelProvider", "reasoningEffort"] {
            if let Some(value) = response.get(key) {
                thread[key] = value.clone();
            }
        }
        let mut session = mapper::session(&thread)?;
        self.apply_usage(&mut session).await;
        self.remember(&session.asid.0).await?;
        let revision = self.metadata_revision(&session.asid.0).await?;
        self.client.snapshot(thread, revision)?;
        Ok(session)
    }

    pub(crate) async fn read_thread(&self, id: &str) -> Result<Value, AgentError> {
        validate_id(id)?;
        let response = self
            .client
            .call("thread/read", json!({"threadId":id,"includeTurns":false}))
            .await?;
        if response["thread"]["id"] != id {
            return Err(AgentError::Protocol(
                "Codex thread/read identity mismatch".into(),
            ));
        }
        Ok(response["thread"].clone())
    }

    fn model_params(params: &mut Value, model: Option<&ModelRef>) -> Result<(), AgentError> {
        if let Some(model) = model {
            if model.provider_id.is_empty() || model.model_id.is_empty() {
                return Err(AgentError::InvalidRequest(
                    "select a model from the Codex catalog".into(),
                ));
            }
            params["model"] = json!(model.model_id);
            params["modelProvider"] = json!(model.provider_id);
            if let Some(effort) = &model.variant {
                if !matches!(
                    effort.as_str(),
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
                ) {
                    return Err(AgentError::InvalidRequest(
                        "invalid Codex reasoning effort".into(),
                    ));
                }
                params["config"] = json!({"model_reasoning_effort":effort});
            }
        }
        Ok(())
    }

    async fn catalog(&self) -> Result<AgentCatalog, AgentError> {
        let mut budget = Budget::new();
        let config = budget
            .call(
                &self.client,
                "config/read",
                json!({"includeLayers":false,"cwd":self.client.config_cwd()}),
            )
            .await?;
        let provider = super::client::provider_from_config(&config)?;
        if self
            .client
            .configured_provider()
            .is_some_and(|startup| startup != provider)
        {
            return Err(AgentError::Unsupported("Codex provider configuration changed; restart Gateway before using its model catalog".into()));
        }
        let mut cursor = None;
        let mut seen = HashSet::new();
        let mut rows = vec![];
        loop {
            let page = budget
                .call(
                    &self.client,
                    "model/list",
                    json!({"limit":100,"cursor":cursor,"includeHidden":false}),
                )
                .await?;
            let data = page["data"]
                .as_array()
                .ok_or_else(|| AgentError::Protocol("invalid Codex model page".into()))?;
            budget.rows(data.len())?;
            rows.extend(data.iter().cloned());
            cursor = next_cursor(&page, &mut seen)?;
            if cursor.is_none() {
                break;
            }
        }
        mapper::catalog(&json!({"data":rows}), &provider)
    }

    async fn validate_model(&self, model: &ModelRef) -> Result<(), AgentError> {
        let catalog = self.catalog().await?;
        let offered = catalog
            .models
            .iter()
            .find(|m| m.provider_id == model.provider_id && m.id == model.model_id)
            .ok_or_else(|| {
                AgentError::InvalidRequest(
                    "select a model from the configured Codex provider catalog".into(),
                )
            })?;
        if model.variant.as_ref().is_some_and(|effort| {
            !offered
                .variants
                .as_ref()
                .is_some_and(|variants| variants.iter().any(|v| &v.id == effort))
        }) {
            return Err(AgentError::InvalidRequest(
                "reasoning effort is not offered for this Codex model".into(),
            ));
        }
        Ok(())
    }

    async fn sessions(&self, query: &SessionQuery) -> Result<Vec<AgentSessionInfo>, AgentError> {
        if query.limit == Some(0) {
            return Ok(vec![]);
        }
        let mut budget = Budget::new();
        let mut seen = HashSet::new();
        if let Some(cursor) = &query.cursor {
            seen.insert(cursor.clone());
        }
        let mut page_query = query.clone();
        let mut out = vec![];
        loop {
            page_query.limit = Some(
                query
                    .limit
                    .map(|n| n.saturating_sub(out.len()).min(100))
                    .unwrap_or(100),
            );
            let page = self.session_page(&page_query, &mut budget).await?;
            out.extend(page.data);
            if query.limit.is_some_and(|n| out.len() >= n) {
                return Ok(out);
            }
            let Some(cursor) = page.cursor.next else {
                return Ok(out);
            };
            if !seen.insert(cursor.clone()) {
                return Err(AgentError::Protocol("Codex session cursor cycle".into()));
            }
            page_query.cursor = Some(cursor);
        }
    }

    async fn session_page(
        &self,
        query: &SessionQuery,
        budget: &mut Budget,
    ) -> Result<SessionPage, AgentError> {
        let order = query.order.as_deref().unwrap_or("desc");
        let limit = query.limit.unwrap_or(50);
        if !matches!(order, "asc" | "desc")
            || !(1..=200).contains(&limit)
            || query
                .cursor
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 8192 || s.chars().any(char::is_control))
        {
            return Err(AgentError::InvalidRequest(
                "invalid Codex session page query".into(),
            ));
        }
        let response = budget.call(&self.client,"thread/list",json!({"cursor":query.cursor,"limit":limit,"sortKey":"updated_at","sortDirection":order,"cwd":query.directory,"searchTerm":query.search,"useStateDbOnly":true,"sourceKinds":["cli","vscode","exec","appServer","subAgent","subAgentReview","subAgentCompact","subAgentThreadSpawn","subAgentOther","unknown"]})).await?;
        let rows = response["data"]
            .as_array()
            .ok_or_else(|| AgentError::Protocol("invalid Codex session page".into()))?;
        if rows.len() > limit {
            return Err(AgentError::Protocol("oversized Codex session page".into()));
        }
        budget.rows(rows.len())?;
        let mut data = vec![];
        for row in rows {
            let mut info = mapper::session(row)?;
            self.apply_usage(&mut info).await;
            if query.keeps_parent(info.parent_id.as_deref()) {
                data.push(info);
            }
        }
        let next = next_cursor(&response, &mut HashSet::new())?;
        Ok(SessionPage {
            data,
            // Native backwardsCursor requires reversing sortDirection; the
            // neutral cursor contract requires reusing the SAME order.
            // Offer correct forward-only pages instead of a broken previous.
            cursor: SessionCursor {
                next,
                previous: None,
            },
        })
    }

    fn request(&self, thread: &str, id: &str) -> Result<super::client::ServerRequest, AgentError> {
        self.client
            .server_requests(thread)
            .into_iter()
            .find(|r| self.client.request_id(&r.id).as_deref() == Some(id))
            .ok_or_else(|| {
                AgentError::InvalidRequest(
                    "Codex request expired, already answered or belongs to another thread".into(),
                )
            })
    }
}

impl AgentPort for CodexDriver {
    fn kind(&self) -> &'static str {
        super::KIND
    }
    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async { Ok(self.client.is_ready()) })
    }
    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::ByPath
    }

    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>> {
        Box::pin(async {
            let sessions = self.sessions(&SessionQuery::default()).await?;
            let mut seen = HashSet::new();
            Ok(sessions
                .into_iter()
                .filter_map(|s| s.directory)
                .filter(|d| seen.insert(d.clone()))
                .map(|d| AgentProject {
                    id: d.clone(),
                    canonical: d.clone(),
                    name: std::path::Path::new(&d)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| d.clone()),
                    vcs: None,
                    sandboxes: vec![],
                    missing: !std::path::Path::new(&d).is_dir(),
                })
                .collect())
        })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async { self.sessions(query).await })
    }

    fn list_sessions_page<'a>(&'a self, query: &'a SessionQuery) -> AgentFuture<'a, SessionPage> {
        Box::pin(async { self.session_page(query, &mut Budget::new()).await })
    }

    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        mode: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if mode.is_some() {
                return unsupported("Codex modes");
            }
            if let Some(directory) = directory {
                if !std::path::Path::new(directory).is_absolute() {
                    return Err(AgentError::InvalidRequest(
                        "Codex cwd must be absolute".into(),
                    ));
                }
                if !std::path::Path::new(directory).is_dir() {
                    return Err(AgentError::WorkspaceMissing(directory.into()));
                }
            }
            let _guard = self.mutations.lock().await;
            let mut params = json!({});
            if let Some(directory) = directory {
                params["cwd"] = json!(directory);
            }
            if let Some(model) = model {
                self.validate_model(model).await?;
            }
            Self::model_params(&mut params, model)?;
            self.selected_response(self.client.call("thread/start", params).await?)
                .await
        })
    }

    fn session_runtime<'a>(&'a self, id: &'a str) -> AgentFuture<'a, Option<SessionRuntime>> {
        Box::pin(async move { Ok(self.runtime(id, false).await) })
    }

    fn get_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let mut info = mapper::session(&self.read_thread(id).await?)?;
            self.apply_usage(&mut info).await;
            Ok(info)
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
            if delivery.is_some() {
                return unsupported("Codex inbox delivery");
            }
            validate_id(id)?;
            if text.is_empty() && attachments.is_empty() {
                return Err(AgentError::InvalidRequest("empty Codex prompt".into()));
            }
            self.prepare_shared_interaction(id).await?;
            let _guard = self.mutations.lock().await;
            self.require_native_interaction(id).await?;
            if self.client.active_turn(id).is_some() {
                return Err(AgentError::InvalidRequest(
                    "Codex thread already has an active turn".into(),
                ));
            }
            if !self.loaded.lock().await.contains(id) {
                let thread = self.read_thread(id).await?;
                if mapper::status(&thread["status"]) == AgentSessionStatus::Busy {
                    return Err(AgentError::InvalidRequest(
                        "Codex thread is already active; not taking it over".into(),
                    ));
                }
                let resumed = self
                    .client
                    .call("thread/resume", json!({"threadId":id,"excludeTurns":true}))
                    .await?;
                if resumed["thread"]["id"] != id {
                    return Err(AgentError::Protocol(
                        "Codex resume identity mismatch".into(),
                    ));
                }
                self.selected_response(resumed).await?;
            }
            let text = append_attachment_paths(text, attachments);
            let mut params =
                json!({"threadId":id,"input":[{"type":"text","text":text,"text_elements":[]}]});
            let model = self
                .next_models
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(id)
                .filter(|choice| choice.accepted_revision.is_none())
                .map(|choice| choice.model.clone());
            if let Some(model) = model {
                let thread = self.read_thread(id).await?;
                if thread["modelProvider"] != model.provider_id {
                    return Err(AgentError::InvalidRequest(
                        "Codex thread provider changed; select a matching configured model".into(),
                    ));
                }
                params["model"] = json!(model.model_id);
                if let Some(effort) = model.variant {
                    params["effort"] = json!(effort);
                }
            }
            let revision = self.metadata_revision(id).await?;
            let response = self.client.call("turn/start", params).await?;
            mapper::string(&response["turn"], "id")?;
            if let Some(choice) = self
                .next_models
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_mut(id)
            {
                if choice.accepted_revision.is_none() {
                    choice.accepted_revision = Some(revision);
                }
            }
            // The accepted turn applies model/effort for subsequent turns too.
            // Re-read stable metadata; never mirror requested values or repeat
            // turn/start when this read fails.
            self.spawn_metadata_refresh(id, revision);
            Ok(())
        })
    }

    fn interrupt<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.prepare_shared_interaction(id).await?;
            self.require_native_interaction(id).await?;
            let turn = self.client.active_turn(id).ok_or_else(|| {
                AgentError::InvalidRequest("Codex thread has no observed active turn".into())
            })?;
            self.client
                .call("turn/interrupt", json!({"threadId":id,"turnId":turn}))
                .await?;
            // The acknowledgement does not mean interrupted. Only the observed
            // turn/completed notification changes the mirror's status.
            Ok(())
        })
    }

    fn switch_model<'a>(&'a self, id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            self.prepare_shared_interaction(id).await?;
            let _guard = self.mutations.lock().await;
            self.require_native_interaction(id).await?;
            let thread = self.read_thread(id).await?;
            if self.client.active_turn(id).is_some()
                || mapper::status(&thread["status"]) == AgentSessionStatus::Busy
            {
                return Err(AgentError::InvalidRequest(
                    "interrupt Codex before switching its model".into(),
                ));
            }
            self.validate_model(model).await?;
            if thread["modelProvider"] != model.provider_id {
                return Err(AgentError::Unsupported(
                    "switching Codex providers on a loaded thread".into(),
                ));
            }
            let mut choices = self.next_models.lock().unwrap_or_else(|p| p.into_inner());
            if choices.len() >= MAX_SESSIONS && !choices.contains_key(id) {
                return Err(AgentError::NotAvailable(
                    "Codex model-selection capacity exhausted".into(),
                ));
            }
            // Resume overrides are ignored for subscribed loaded threads.
            // Stage a validated choice for real turn/start overrides instead;
            // never publish a fabricated upstream model selection snapshot.
            choices.insert(
                id.into(),
                PendingModel {
                    model: model.clone(),
                    accepted_revision: None,
                },
            );
            Ok(())
        })
    }

    fn rename_session<'a>(&'a self, id: &'a str, title: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            validate_id(id)?;
            self.client
                .call("thread/name/set", json!({"threadId":id,"name":title}))
                .await?;
            Ok(())
        })
    }

    fn delete_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            validate_id(id)?;
            self.prepare_shared_interaction(id).await?;
            self.require_native_interaction(id).await?;
            if self.client.active_turn(id).is_some() {
                return Err(AgentError::InvalidRequest(
                    "interrupt Codex before deleting its thread".into(),
                ));
            }
            self.client
                .call("thread/delete", json!({"threadId":id}))
                .await?;
            self.forget_thread(id).await;
            self.clear_usage(id).await;
            Ok(())
        })
    }

    fn fork_session<'a>(
        &'a self,
        id: &'a str,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            validate_id(id)?;
            if message.is_some() {
                return unsupported("Codex item-boundary fork (only whole-thread fork is exposed)");
            }
            self.selected_response(
                self.client
                    .call("thread/fork", json!({"threadId":id,"excludeTurns":true}))
                    .await?,
            )
            .await
        })
    }

    fn reply_permission<'a>(
        &'a self,
        thread: &'a str,
        id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if message.is_some() {
                return unsupported("Codex approval rejection reason");
            }
            if decision == PermissionDecision::AllowAlways {
                return unsupported("Codex permanent approval through neutral AllowAlways");
            }
            let request = self.request(thread, id)?;
            if mapper::permission(&request.id, &request.method, &request.params).is_none() {
                return unsupported("Codex approval type");
            }
            let result = if request.method == "item/permissions/requestApproval" {
                let mut permissions = serde_json::Map::new();
                if decision == PermissionDecision::Allow {
                    for key in ["network", "fileSystem"] {
                        if let Some(value) = request.params["permissions"]
                            .get(key)
                            .filter(|v| !v.is_null())
                        {
                            permissions.insert(key.into(), value.clone());
                        }
                    }
                }
                json!({"permissions":permissions,"scope":"turn"})
            } else {
                json!({"decision":if decision == PermissionDecision::Allow {"accept"} else {"decline"}})
            };
            self.client.reply(thread, id, result).await
        })
    }

    fn reply_form<'a>(
        &'a self,
        thread: &'a str,
        id: &'a str,
        answers: Value,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let request = self.request(thread, id)?;
            let form =
                mapper::form(&request.id, &request.method, &request.params).ok_or_else(|| {
                    AgentError::Unsupported("Codex secret or complex user input".into())
                })?;
            let supplied = answers.as_object().ok_or_else(|| {
                AgentError::InvalidRequest("Codex answers must be an object".into())
            })?;
            let mut result = serde_json::Map::new();
            for field in &form.fields {
                let FormField::String {
                    key,
                    options,
                    custom,
                    ..
                } = field
                else {
                    return unsupported("Codex form field");
                };
                let answer = supplied.get(key).and_then(Value::as_str).ok_or_else(|| {
                    AgentError::InvalidRequest("Codex question requires a string answer".into())
                })?;
                if !*custom && !options.is_empty() && !options.iter().any(|o| o.value == answer) {
                    return Err(AgentError::InvalidRequest(
                        "Codex answer is not an offered option".into(),
                    ));
                }
                result.insert(key.clone(), json!({"answers":[answer]}));
            }
            if supplied.len() != result.len() {
                return Err(AgentError::InvalidRequest(
                    "unexpected Codex answer fields".into(),
                ));
            }
            self.client
                .reply(thread, id, json!({"answers":result}))
                .await
        })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            Ok(self
                .client
                .server_requests(id)
                .iter()
                .filter_map(|r| self.permission(&r.id, &r.method, &r.params))
                .collect())
        })
    }
    fn get_pending_forms<'a>(&'a self, id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async move {
            Ok(self
                .client
                .server_requests(id)
                .iter()
                .filter_map(|r| self.form(&r.id, &r.method, &r.params))
                .collect())
        })
    }

    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move { self.catalog().await })
    }

    fn get_timeline<'a>(&'a self, id: &'a str, limit: usize) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            if limit == 0 {
                return Ok(vec![]);
            }
            if limit > MAX_HISTORY_ITEMS {
                return Err(AgentError::InvalidRequest(
                    "Codex history limit exceeds 2000 mapped rows".into(),
                ));
            }
            validate_id(id)?;
            let mut budget = Budget::new();
            let metadata = budget
                .call(
                    &self.client,
                    "thread/read",
                    json!({"threadId":id,"includeTurns":false}),
                )
                .await?;
            if metadata["thread"]["id"] != id {
                return Err(AgentError::Protocol(
                    "Codex history identity mismatch".into(),
                ));
            }
            let thread = &metadata["thread"];
            self.rejoin_loaded(id, thread).await?;
            let prepared = super::rollout::Rollouts::prepare(self, thread).await;
            let mut items = vec![];
            match thread["historyMode"].as_str() {
                Some("paginated") => {
                    let mut rows = vec![];
                    let mut cursor = None;
                    let mut seen = HashSet::new();
                    let mut mapped = 0;
                    loop {
                        let page_limit = (limit - mapped).min(100);
                        let page = budget.call(&self.client,"thread/items/list",json!({"threadId":id,"limit":page_limit,"sortDirection":"desc","cursor":cursor})).await?;
                        let data = page["data"].as_array().ok_or_else(|| {
                            AgentError::Protocol("invalid Codex item page".into())
                        })?;
                        if data.len() > page_limit {
                            return Err(AgentError::Protocol("oversized Codex item page".into()));
                        }
                        budget.rows(data.len())?;
                        for row in data {
                            let turn = mapper::string(row, "turnId")?;
                            mapper::string(&row["item"], "id")?;
                            mapper::string(&row["item"], "type")?;
                            if row["item"]["type"] == "reasoning"
                                && ["summary", "content"].iter().any(|key| {
                                    row["item"][*key].as_array().is_some_and(|parts| {
                                        parts.len() > super::MAX_REASONING_PARTS
                                    })
                                })
                            {
                                return Err(AgentError::Protocol(
                                    "Codex reasoning part limit exceeded".into(),
                                ));
                            }
                            mapped += mapper::item(id, &turn, &row["item"], 0).len();
                        }
                        rows.extend(data.iter().cloned());
                        cursor = next_cursor(&page, &mut seen)?;
                        if mapped >= limit || cursor.is_none() {
                            break;
                        }
                        if data.is_empty() {
                            return Err(AgentError::Protocol(
                                "empty continuing Codex item page".into(),
                            ));
                        }
                    }
                    let mut ids_by_turn: HashMap<String, Vec<String>> = HashMap::new();
                    for row in rows.iter().rev() {
                        ids_by_turn
                            .entry(mapper::string(row, "turnId")?)
                            .or_default()
                            .push(mapper::string(&row["item"], "id")?);
                    }
                    for (turn, ids) in ids_by_turn {
                        self.set_history_order(id, &turn, &ids)?;
                    }
                    for row in rows.iter().rev() {
                        let turn = mapper::string(row, "turnId")?;
                        items.extend(
                            self.ordered_items(
                                id,
                                &turn,
                                &row["item"],
                                row["startedAtMs"]
                                    .as_u64()
                                    .or_else(|| row["completedAtMs"].as_u64())
                                    .unwrap_or(0),
                            )?,
                        );
                    }
                }
                Some("legacy") => {
                    let response = budget
                        .call(
                            &self.client,
                            "thread/read",
                            json!({"threadId":id,"includeTurns":true}),
                        )
                        .await?;
                    if response["thread"]["id"] != id {
                        return Err(AgentError::Protocol(
                            "Codex history identity mismatch".into(),
                        ));
                    }
                    let turns = response["thread"]["turns"].as_array().ok_or_else(|| {
                        AgentError::Protocol("invalid Codex legacy history".into())
                    })?;
                    for turn in turns {
                        let turn_id = mapper::string(turn, "id")?;
                        if let Some(rows) = turn["items"].as_array() {
                            budget.rows(rows.len())?;
                            let ids = rows
                                .iter()
                                .map(|row| mapper::string(row, "id"))
                                .collect::<Result<Vec<_>, _>>()?;
                            self.set_history_order(id, &turn_id, &ids)?;
                            for row in rows {
                                items.extend(self.ordered_items(
                                    id,
                                    &turn_id,
                                    row,
                                    turn["startedAt"].as_u64().unwrap_or(0).saturating_mul(1000),
                                )?);
                            }
                        }
                    }
                }
                _ => return unsupported("Codex thread history storage mode"),
            }
            items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
            if items.len() > limit {
                items.drain(..items.len() - limit);
            }
            if let Some(prepared) = prepared {
                if let Some(value) = &prepared.usage {
                    let mut usage = self.usage.lock().await;
                    if usage.len() < MAX_SESSIONS || usage.contains_key(id) {
                        usage
                            .entry(id.into())
                            .or_insert_with(|| super::usage::Usage::from_wire(value));
                    }
                }
                self.rollouts.install(id, prepared);
            }
            Ok(items)
        })
    }

    fn revert_session<'a>(&'a self, _id: &'a str, _message: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { unsupported("Codex file-restoring revert") })
    }
    fn switch_mode<'a>(&'a self, _id: &'a str, _mode: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { unsupported("Codex modes") })
    }
    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async { unsupported("Codex file search") })
    }
    fn get_vcs_diff<'a>(
        &'a self,
        _id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async { unsupported("Codex VCS diff") })
    }
}

#[cfg(test)]
mod model_page_tests {
    use super::super::tests::{golden, Fixture};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn codex_model_page_pending_selection_survives_failed_read_until_source_confirmation() {
        let accepted = Arc::new(AtomicBool::new(false));
        let state = accepted.clone();
        let recovered = Arc::new(AtomicBool::new(false));
        let recovery = recovered.clone();
        let mut fixture = Fixture::new(move |r| match r["method"].as_str().unwrap() {
            "config/read" => json!({"config":{"model_provider":"openai"}}),
            "model/list" => golden()["modelList"].clone(),
            "thread/start" => json!({"thread":golden()["thread"]}),
            "thread/read" => {
                if state.load(Ordering::Acquire) && !recovery.load(Ordering::Acquire) {
                    return json!({"thread":{"id":"synthetic-wrong"}});
                }
                let mut thread = golden()["thread"].clone();
                if recovery.load(Ordering::Acquire) {
                    thread["model"] = json!("synthetic-normalized");
                    thread["reasoningEffort"] = json!("low");
                }
                json!({"thread":thread})
            }
            "turn/start" => {
                assert_eq!(r["params"]["model"], "fixture-model");
                assert_eq!(r["params"]["effort"], "high");
                state.store(true, Ordering::Release);
                json!({"turn":{"id":"synthetic-turn","status":"inProgress"}})
            }
            _ => panic!("unexpected pending-selection fixture RPC"),
        })
        .await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let initial = driver.create_session(None, None, None).await.unwrap();
        let choice = ModelRef {
            provider_id: "openai".into(),
            model_id: "fixture-model".into(),
            variant: Some("high".into()),
        };
        driver.switch_model(&initial.asid.0, &choice).await.unwrap();
        driver
            .send_prompt(&initial.asid.0, "synthetic accepted prompt", &[], None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    fixture.events.as_mut().unwrap().recv().await.unwrap(),
                    super::super::CodexEvent::MetadataRefreshFailed { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        {
            let choices = driver.next_models.lock().unwrap();
            let pending = choices.get(&initial.asid.0).unwrap();
            assert_eq!(pending.model, choice);
            assert!(
                pending.accepted_revision.is_some(),
                "accepted choice is retained but must not be resent"
            );
        }
        recovered.store(true, Ordering::Release);
        driver.refresh_metadata(&initial.asid.0).await.unwrap();
        let event = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.events.as_mut().unwrap().recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(event, super::super::CodexEvent::Snapshot {thread,..} if thread["model"] == "synthetic-normalized" && thread["reasoningEffort"] == "low")
        );
        assert!(
            driver.next_models.lock().unwrap().is_empty(),
            "actual source confirms even a normalized selection"
        );
    }

    #[tokio::test]
    async fn codex_model_page_waiting_refresh_workers_are_bounded_and_cancelled() {
        let mut fixture = Fixture::new(|_| panic!("blocked workers must not dispatch RPCs")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let guard = driver.mutations.lock().await;
        for _ in 0..33 {
            driver.refresh_metadata("synthetic-thread").await.unwrap();
        }
        assert!(
            !fixture.client.is_ready(),
            "metadata worker overload fails closed"
        );
        drop(guard);
        drop(driver);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&fixture.client) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(fixture.requests.try_recv().is_err());
    }
}
