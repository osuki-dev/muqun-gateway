use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::{
    budget::{next_cursor, Budget},
    inbox::{Delivery, Inbox, InboxSignal, Queued},
    mapper, CodexClient,
};
use crate::agents::domain::*;
use crate::agents::ports::agent::{
    AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem, HistoryCommit,
    PreparedTerminalLaunch, PreparedTimeline,
};

const MAX_SESSIONS: usize = 4096;
const MAX_HISTORY_ITEMS: usize = 2000;
const MAX_HISTORY_RETAINED_BYTES: usize = 32 * 1024 * 1024;
// Leave room to insert an older authoritative prefix before a live/tail anchor.
const FIRST_ITEM_POSITION: u64 = 1 << 63;
// Live observation can skip items that a later authoritative page fills in.
const ITEM_POSITION_STEP: u64 = 1 << 32;

type ItemOrder = HashMap<(String, String, String), u64>;

#[derive(Debug)]
enum OrderError {
    RebaseRequired,
    Invalid(AgentError),
}

impl From<AgentError> for OrderError {
    fn from(error: AgentError) -> Self {
        Self::Invalid(error)
    }
}

impl OrderError {
    fn into_agent(self) -> AgentError {
        match self {
            Self::RebaseRequired => AgentError::HistoryCursorLost,
            Self::Invalid(error) => error,
        }
    }
}

pub struct CodexDriver {
    client: Arc<CodexClient>,
    account_limits: super::account_limits::Cache,
    terminal_runtime: Option<Arc<crate::agents::terminal_runtime::TerminalRuntime>>,
    native_sessions: Option<Arc<super::native_sessions::NativeSessions>>,
    /// Only explicitly created, forked or submitted-to threads are resumed.
    loaded: Arc<Mutex<HashSet<String>>>,
    question_recoveries: Arc<Mutex<QuestionRecoveries>>,
    /// Serializes explicit mutations, not the stream or read-only RPC calls.
    mutations: Arc<Mutex<()>>,
    order: std::sync::Mutex<ItemOrder>,
    order_epochs: std::sync::Mutex<HashMap<String, u64>>,
    next_models: Arc<std::sync::Mutex<HashMap<String, PendingModel>>>,
    metadata_versions: Arc<Mutex<HashMap<String, u64>>>,
    next_metadata_version: std::sync::atomic::AtomicU64,
    refresh_slots: Arc<tokio::sync::Semaphore>,
    usage: Mutex<HashMap<String, super::usage::Usage>>,
    pub(crate) rollouts: super::rollout::Rollouts,
    async_questions: Mutex<super::async_questions::Registry>,
    /// Set only after `skills/list` returns a skills list. A missing method
    /// or a bad payload leaves this false and the catalog skills empty.
    skills_catalog: std::sync::atomic::AtomicBool,
    /// Skill name (lowercased) to the `SKILL.md` path from the last list.
    /// Codex turns a skill on with that path; the name alone is not enough.
    skill_paths: std::sync::Mutex<HashMap<String, String>>,
    /// Serializes provider page reads, not a prepared tail's mirror ACK.
    history: Mutex<()>,
    /// Synchronous revision-checked publication, also used inside mirror acceptance.
    history_state: std::sync::Mutex<HistoryState>,
    history_preparations: Arc<tokio::sync::Semaphore>,
    /// Set only when the downloaded Codex command catalog contributed rows.
    slash_catalog: std::sync::atomic::AtomicBool,
    /// Prompts waiting for a thread's turn to end. Changed only while holding
    /// `mutations`, so a delivery and a cancel never race for one item.
    inbox: std::sync::Mutex<Inbox>,
    inbox_signals: Option<tokio::sync::mpsc::UnboundedSender<InboxSignal>>,
}

/// Why a prompt did not reach Codex. `Unknown` means it may have: the request
/// was written and no definite answer came back. It is never resent.
enum SendFailure {
    NotSent(AgentError),
    Unknown(AgentError),
}

impl SendFailure {
    /// Codex answered with an error, so the input was not accepted. Anything
    /// else after the request was written is an unknown outcome.
    fn after_write(error: AgentError) -> Self {
        match error {
            AgentError::RequestFailed(_) => Self::NotSent(error),
            _ => Self::Unknown(error),
        }
    }

    fn into_error(self) -> AgentError {
        match self {
            Self::NotSent(error) | Self::Unknown(error) => error,
        }
    }
}

fn inbox_item_not_found(id: &str) -> AgentError {
    AgentError::InboxItemNotFound(format!("Codex inbox item {id} is not queued"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct QuestionRecoveryKey {
    thread: String,
    turn: String,
    ownership: u64,
}

struct QuestionRecovery {
    key: QuestionRecoveryKey,
    history_mode: Option<String>,
}

#[derive(Default)]
struct QuestionRecoveries {
    scheduled: HashSet<QuestionRecoveryKey>,
    pending: VecDeque<QuestionRecovery>,
    workers: usize,
}

struct PendingModel {
    model: ModelRef,
    accepted_revision: Option<u64>,
}

#[derive(Clone, Default)]
struct HistoryPage {
    revision: u64,
    started: bool,
    legacy_turns: bool,
    cursor: Option<String>,
    /// Raw page while preparing, oldest first. Shared pages are projected once
    /// before publication; private stdio keeps its existing one-item expansion.
    pending: Vec<Value>,
    /// Older mapped prefix, without retaining/remapping a large raw image item.
    remaining: Vec<TimelineItem>,
    seen: HashSet<String>,
    retained_bytes: usize,
    /// When this page was last committed (a snapshot or an earlier batch
    /// read), for evicting the least recently read session first.
    read_at_ms: u64,
}

#[derive(Default)]
struct HistoryState {
    pages: HashMap<String, HistoryPage>,
    /// Monotonic across session deletion/recreation, issued under this lock.
    revision: u64,
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

fn skill_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn skill_path_ok(path: &str) -> bool {
    path.starts_with('/') && path.len() <= 4096 && !path.chars().any(char::is_control)
}

impl CodexDriver {
    pub(crate) fn invalidate_account_limits(&self) {
        self.account_limits.invalidate();
    }

    pub fn new(client: Arc<CodexClient>) -> Self {
        Self {
            client,
            account_limits: Default::default(),
            terminal_runtime: None,
            native_sessions: None,
            loaded: Arc::new(Mutex::new(HashSet::new())),
            question_recoveries: Arc::new(Mutex::new(QuestionRecoveries::default())),
            mutations: Arc::new(Mutex::new(())),
            order: std::sync::Mutex::new(HashMap::new()),
            order_epochs: std::sync::Mutex::new(HashMap::new()),
            next_models: Arc::new(std::sync::Mutex::new(HashMap::new())),
            metadata_versions: Arc::new(Mutex::new(HashMap::new())),
            next_metadata_version: std::sync::atomic::AtomicU64::new(1),
            refresh_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            usage: Mutex::new(HashMap::new()),
            rollouts: Default::default(),
            async_questions: Mutex::new(super::async_questions::Registry::default()),
            skills_catalog: std::sync::atomic::AtomicBool::new(false),
            skill_paths: std::sync::Mutex::new(HashMap::new()),
            history: Mutex::new(()),
            history_state: std::sync::Mutex::new(HistoryState::default()),
            history_preparations: Arc::new(tokio::sync::Semaphore::new(4)),
            slash_catalog: std::sync::atomic::AtomicBool::new(false),
            inbox: std::sync::Mutex::new(Inbox::default()),
            inbox_signals: None,
        }
    }

    /// Where queue changes are published. Without it the queue still works,
    /// but nothing reaches the mirror or the stream.
    pub(crate) fn observe_inbox(
        &mut self,
        signals: tokio::sync::mpsc::UnboundedSender<InboxSignal>,
    ) {
        self.inbox_signals = Some(signals);
    }

    fn signal(&self, signal: InboxSignal) {
        if let Some(signals) = &self.inbox_signals {
            let _ = signals.send(signal);
        }
    }

    /// Caller holds `mutations`; all queue receipts use one publication path.
    fn enqueue_prompt(
        &self,
        thread: &str,
        text: &str,
        files: Vec<Value>,
        input: Vec<Value>,
        delivery: Delivery,
    ) -> Result<(), AgentError> {
        self.queue().push(
            thread,
            Queued {
                id: format!("inbox_{}", uuid::Uuid::new_v4().simple()),
                created_ms: now_ms(),
                delivery,
                text: text.to_owned(),
                files,
                input,
            },
        )?;
        self.signal(InboxSignal::Changed(thread.into()));
        Ok(())
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.inbox.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn inbox_items(&self, thread: &str) -> Vec<Value> {
        self.queue().items(thread)
    }

    /// A deleted thread's queue goes with it, without publishing.
    pub(crate) fn clear_inbox(&self, thread: &str) {
        self.queue().clear(thread);
    }

    /// Record a send attempt's outcome for a queued item. Called while still
    /// holding `mutations`, so no other delivery can pick the same item.
    fn settle(
        &self,
        thread: &str,
        id: &str,
        result: Result<(), SendFailure>,
    ) -> Result<(), AgentError> {
        match result {
            Ok(()) => {
                self.queue().remove(thread, id);
                self.signal(InboxSignal::Changed(thread.into()));
                Ok(())
            }
            Err(SendFailure::NotSent(error)) => {
                tracing::warn!(
                    %error,
                    "Codex did not accept a queued prompt; it stays in the inbox"
                );
                Err(error)
            }
            Err(SendFailure::Unknown(error)) => {
                tracing::warn!(
                    %error,
                    "A queued Codex prompt may have been delivered; removed and not resent"
                );
                self.queue().remove(thread, id);
                self.signal(InboxSignal::Changed(thread.into()));
                self.signal(InboxSignal::Uncertain(thread.into()));
                Err(error)
            }
        }
    }

    /// After a turn completed normally, hand the next queued prompt to Codex
    /// as a new turn. Interrupted or failed turns never call this.
    pub(crate) fn deliver_queued(self: &Arc<Self>, thread: &str) {
        // Not checked here: a prompt queued while this completion was in
        // flight is pushed under `mutations`, which `deliver_next` waits for.
        let driver = self.clone();
        let thread = thread.to_owned();
        tokio::spawn(async move { driver.deliver_next(&thread).await });
    }

    async fn deliver_next(&self, thread: &str) {
        let _guard = self.mutations.lock().await;
        // Another prompt (or another client) already started the next turn.
        if self.client.active_turn(thread).is_some() {
            return;
        }
        let Some(item) = self.queue().next(thread).cloned() else {
            return;
        };
        let result = self.start_turn_locked(thread, item.input).await;
        let _ = self.settle(thread, &item.id, result);
    }

    /// Compaction postpones steering; it must not turn it into next-turn work.
    pub(crate) fn deliver_deferred_steering(self: &Arc<Self>, thread: &str) {
        let driver = self.clone();
        let thread = thread.to_owned();
        tokio::spawn(async move {
            let _guard = driver.mutations.lock().await;
            while !driver.client.is_compacting(&thread) {
                let Some(item) = driver.queue().next(&thread).cloned() else {
                    break;
                };
                if item.delivery != Delivery::Steer {
                    break;
                }
                let result = match driver.client.active_turn(&thread) {
                    Some(turn) => driver.steer_locked(&thread, &turn, item.input).await,
                    None => driver.start_turn_locked(&thread, item.input).await,
                };
                if driver.settle(&thread, &item.id, result).is_err() {
                    break;
                }
            }
        });
    }

    /// "Send now": steer it into the running turn, or start a turn with it.
    async fn send_queued_now(&self, thread: &str, id: &str) -> Result<(), AgentError> {
        self.prepare_shared_interaction(thread).await?;
        let _guard = self.mutations.lock().await;
        let input = self
            .queue()
            .get(thread, id)
            .map(|item| item.input.clone())
            .ok_or_else(|| inbox_item_not_found(id))?;
        if self.client.is_compacting(thread) {
            // Preserve the requested mode while compaction blocks delivery.
            self.queue().set_delivery(thread, id, Delivery::Steer);
            self.signal(InboxSignal::Changed(thread.into()));
            return Ok(());
        }
        let result = match self.client.active_turn(thread) {
            Some(turn) => self.steer_locked(thread, &turn, input).await,
            None => self.start_turn_locked(thread, input).await,
        };
        self.settle(thread, id, result)
    }

    /// Steer an owned live turn. Caller holds `mutations`.
    async fn steer_locked(
        &self,
        thread: &str,
        turn: &str,
        input: Vec<Value>,
    ) -> Result<(), SendFailure> {
        self.require_native_interaction(thread)
            .await
            .map_err(SendFailure::NotSent)?;
        if !self.loaded.lock().await.contains(thread) {
            return Err(SendFailure::NotSent(AgentError::Unsupported(
                "Codex steering requires an owned live thread".into(),
            )));
        }
        if !self.client.is_ready() {
            return Err(SendFailure::NotSent(AgentError::NotAvailable(
                "Codex is not connected; nothing was sent".into(),
            )));
        }
        self.steer(thread, turn, input)
            .await
            .map_err(SendFailure::after_write)
    }

    pub(crate) fn observe_terminals(
        &mut self,
        observer: Option<Arc<crate::agents::terminal_runtime::TerminalRuntime>>,
    ) {
        self.terminal_runtime = observer;
    }

    pub(crate) fn restore_native_sessions(
        &mut self,
        sessions: Option<Arc<super::native_sessions::NativeSessions>>,
    ) {
        self.native_sessions = sessions;
    }

    async fn record_native_acceptance(&self, id: &str) {
        let (Some(sessions), Some(home)) = (&self.native_sessions, self.client.codex_home()) else {
            return;
        };
        // Upstream has already accepted the mutation. A local storage failure
        // must not turn that success into a failed send that the App retries.
        if sessions.record(home, id).await.is_err() {
            tracing::warn!("Could not persist accepted native Codex session evidence");
        }
    }

    async fn can_rejoin_selected(&self, id: &str, metadata: &Value) -> bool {
        match metadata["status"]["type"].as_str() {
            Some("idle" | "active") => true,
            Some("notLoaded") => {
                let (Some(sessions), Some(home)) =
                    (&self.native_sessions, self.client.codex_home())
                else {
                    return false;
                };
                sessions.contains(home, id).await.unwrap_or(false)
            }
            _ => false,
        }
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
            self.rejoin_loaded(id, &thread, false).await?;
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
            // A cache of each session's last usage: another session's entry
            // goes, never the connection.
            if let Some(evicted) = usage.keys().next().cloned() {
                usage.remove(&evicted);
            }
        }
        usage.insert(id.into(), super::usage::Usage::from_wire(value));
        Ok(())
    }

    pub(crate) async fn clear_usage(&self, id: &str) {
        self.usage.lock().await.remove(id);
    }

    async fn seed_usage(&self, id: &str, value: Option<&Value>, external: bool) {
        let mut usage = self.usage.lock().await;
        // An explicit external reread may select a reverted rollout. Its sample
        // supersedes earlier persisted counts; no sample means unknown. Native
        // live notifications remain stronger than a historical initial sample.
        if external {
            if usage.len() < MAX_SESSIONS || usage.contains_key(id) {
                usage.insert(
                    id.into(),
                    super::usage::Usage::from_wire(value.unwrap_or(&Value::Null)),
                );
            }
            return;
        }
        if usage.len() < MAX_SESSIONS || usage.contains_key(id) {
            if let Some(value) = value {
                usage
                    .entry(id.into())
                    .or_insert_with(|| super::usage::Usage::from_wire(value));
            }
        }
    }

    pub(crate) async fn cached_usage(&self, id: &str) -> Option<super::usage::Usage> {
        self.usage.lock().await.get(id).cloned()
    }

    pub(crate) async fn apply_usage(&self, info: &mut AgentSessionInfo) {
        if let Some(usage) = self.usage.lock().await.get(&info.asid.0) {
            usage.apply(info);
        }
    }

    pub(crate) async fn observe_async_questions(
        &self,
        thread: &str,
        turn: &str,
        item: &Value,
    ) -> Vec<FormRequest> {
        if item["type"] != "agentMessage"
            || !self.client.is_ready()
            || self.client.active_turn(thread).as_deref() != Some(turn)
            || !self.loaded.lock().await.contains(thread)
            // Stream observation must not queue behind a terminal backend read.
            // Actual replies still refresh and recheck native ownership.
            || self
                .cached_runtime(thread)
                .await
                .is_some_and(|runtime| runtime.interaction != RuntimeInteraction::Native)
        {
            return vec![];
        }
        self.async_questions
            .lock()
            .await
            .insert(&self.client, thread, turn, item)
    }
    pub(crate) async fn observe_async_answers(
        &self,
        thread: &str,
        turn: &str,
        item: &Value,
    ) -> Vec<String> {
        if item["type"] != "userMessage" {
            return vec![];
        }
        self.async_questions
            .lock()
            .await
            .observe_reply(thread, turn, &item["content"])
    }
    pub(crate) async fn finish_async_questions(
        &self,
        thread: &str,
        turn: &str,
    ) -> HashMap<String, FormResolutionReason> {
        let ownership = self.client.subscription_revision(thread);
        let loaded = self.loaded.lock().await.contains(thread);
        let owned = self.client.is_ready()
            && (!self.client.is_shared() || ownership.is_some())
            && loaded
            && self
                .cached_runtime(thread)
                .await
                .is_none_or(|runtime| runtime.interaction == RuntimeInteraction::Native);
        let mut questions = self.async_questions.lock().await;
        // Ownership can change while local state locks are awaited. Recheck at
        // classification so an unload/disconnect never becomes draft expiry.
        let owned = owned
            && self.client.is_ready()
            && self.client.subscription_revision(thread) == ownership
            && self
                .client
                .active_turn(thread)
                .is_none_or(|active| active == turn);
        questions.finish_turn(thread, turn, ownership, owned)
    }

    pub(crate) async fn clear_async_questions(&self, thread: &str) {
        self.async_questions.lock().await.clear(thread);
    }
    async fn steer(&self, thread: &str, turn: &str, input: Vec<Value>) -> Result<(), AgentError> {
        let result = self
            .client
            .call(
                "turn/steer",
                json!({"threadId":thread,"expectedTurnId":turn,"input":input}),
            )
            .await?;
        if result["turnId"] != turn {
            self.client.stop();
            return Err(AgentError::Protocol(
                "Codex steering turn identity mismatch".into(),
            ));
        }
        Ok(())
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
            return Err(AgentError::NotAvailable(
                "Codex item-order capacity exhausted; resync required".into(),
            ));
        }
        let position = match order.get(&key).copied() {
            Some(position) => position,
            None => order
                .iter()
                .filter(|((t, u, _), _)| t == thread && u == turn)
                .map(|(_, position)| *position)
                .max()
                .map(|position| position.checked_add(ITEM_POSITION_STEP))
                .unwrap_or(Some(FIRST_ITEM_POSITION))
                .ok_or_else(|| {
                    AgentError::Protocol("Codex item-order range exhausted; resync required".into())
                })?,
        };
        order.insert(key, position);
        Ok(position)
    }

    #[cfg(test)]
    fn set_history_order(
        &self,
        thread: &str,
        turn: &str,
        ids: &[String],
    ) -> Result<(), AgentError> {
        self.set_history_order_at(thread, turn, ids, false)
    }

    #[cfg(test)]
    fn set_history_order_at(
        &self,
        thread: &str,
        turn: &str,
        ids: &[String],
        prefix: bool,
    ) -> Result<(), AgentError> {
        Self::assign_history_order(
            &mut self.order.lock().unwrap_or_else(|p| p.into_inner()),
            thread,
            turn,
            ids,
            prefix,
        )
        .map_err(OrderError::into_agent)
    }

    fn assign_history_order(
        order: &mut ItemOrder,
        thread: &str,
        turn: &str,
        ids: &[String],
        prefix: bool,
    ) -> Result<(), OrderError> {
        if ids.is_empty() {
            return Ok(());
        }

        let key = |id: &str| (thread.to_string(), turn.to_string(), id.to_string());
        let range_error = || OrderError::RebaseRequired;
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(
                AgentError::Protocol("duplicate Codex canonical item identity".into()).into(),
            );
        }
        let new_ids: HashSet<_> = ids
            .iter()
            .filter(|id| !order.contains_key(&key(id)))
            .collect();
        if order.len() + new_ids.len() > 8192 {
            // A historical page failing local retention is not a control/live
            // transport failure. Leave the old order/cursor and primary intact.
            return Err(AgentError::RequestFailed(
                "Codex history item-order capacity exhausted; page not consumed".into(),
            )
            .into());
        }
        let mut positions: Vec<_> = ids.iter().map(|id| order.get(&key(id)).copied()).collect();
        let anchors: Vec<_> = positions.iter().flatten().copied().collect();
        if anchors.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(range_error());
        }
        let mut occupied: std::collections::BTreeSet<u64> = order
            .iter()
            .filter(|((t, u, _), _)| t == thread && u == turn)
            .map(|(_, p)| *p)
            .collect();
        let mut cursor = 0;
        while cursor < positions.len() {
            if positions[cursor].is_some() {
                cursor += 1;
                continue;
            }
            let start = cursor;
            while cursor < positions.len() && positions[cursor].is_none() {
                cursor += 1;
            }
            let count = cursor - start;
            let mut left = start.checked_sub(1).and_then(|i| positions[i]);
            let mut right = positions.get(cursor).copied().flatten();
            // A disjoint page belongs before/after the retained range. An
            // overlapping page uses all known anchors, not a contiguous offset.
            match (left, right) {
                (None, None) if prefix => right = occupied.first().copied(),
                (None, None) => left = occupied.last().copied(),
                (None, Some(upper)) => left = occupied.range(..upper).next_back().copied(),
                (Some(lower), None) => {
                    right = occupied
                        .range((std::ops::Bound::Excluded(lower), std::ops::Bound::Unbounded))
                        .next()
                        .copied()
                }
                _ => {}
            }
            let step = i128::from(ITEM_POSITION_STEP);
            let count = count as i128;
            let (base, stride, upper) = match (left, right) {
                (Some(lower), Some(upper)) => {
                    let gap = i128::from(upper) - i128::from(lower);
                    (i128::from(lower), gap / (count + 1), i128::from(upper))
                }
                (None, Some(upper)) => {
                    let stride = step.min((i128::from(upper) + 1) / (count + 1));
                    (
                        i128::from(upper) - stride * (count + 1),
                        stride,
                        i128::from(upper),
                    )
                }
                (Some(lower), None) => (i128::from(lower), step, i128::from(u64::MAX) + 1),
                (None, None) => (
                    i128::from(FIRST_ITEM_POSITION) - step,
                    step,
                    i128::from(u64::MAX) + 1,
                ),
            };
            if stride < 1 {
                return Err(range_error());
            }
            let mut previous = base;
            for (offset, slot) in positions[start..cursor].iter_mut().enumerate() {
                let mut candidate = (base + stride * (offset as i128 + 1)).max(previous + 1);
                while candidate < upper
                    && u64::try_from(candidate).is_ok_and(|p| occupied.contains(&p))
                {
                    candidate += 1;
                }
                if candidate >= upper {
                    return Err(range_error());
                }
                let position = u64::try_from(candidate).map_err(|_| range_error())?;
                occupied.insert(position);
                *slot = Some(position);
                previous = candidate;
            }
        }
        // Commit only after every run is valid: failed pages cannot poison the
        // retained order or change an already displayed row's sort key.
        for (id, position) in ids.iter().zip(positions) {
            order
                .entry(key(id))
                .or_insert(position.expect("all runs assigned"));
        }
        Ok(())
    }

    /// A complete producer array supplies order before any missing items are
    /// projected. Arrival order alone cannot place items missed during reconnect.
    #[cfg(test)]
    fn ordered_turn_items(
        &self,
        thread: &str,
        turn: &str,
        items: &[Value],
    ) -> Result<Vec<TimelineItem>, AgentError> {
        let ids = items
            .iter()
            .map(|item| mapper::string(item, "id"))
            .collect::<Result<Vec<_>, _>>()?;
        self.set_history_order(thread, turn, &ids)?;
        let mut rows = Vec::new();
        for item in items {
            rows.extend(self.ordered_items(thread, turn, item, 0)?);
        }
        Ok(rows)
    }

    /// Complete turn arrays can repair provisional live order. Their rows and
    /// the projection commit are accepted together by the mirror.
    pub(crate) fn prepare_completed_turn<'a>(
        &'a self,
        thread: &'a str,
        turn: &str,
        items: &[Value],
        full: bool,
    ) -> Result<PreparedTimeline<'a>, AgentError> {
        let (mut order, epoch) = self.order_candidate(thread);
        let ids = items
            .iter()
            .map(|item| mapper::string(item, "id"))
            .collect::<Result<Vec<_>, _>>()?;
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(AgentError::Protocol(
                "duplicate Codex completion item identity".into(),
            ));
        }
        if full {
            Self::rebase_prepared_order(thread, turn, &ids, &mut order)?;
        } else {
            // A summary is not canonical turn order. Retain known positions
            // and append previously unseen items without erasing other rows.
            for id in &ids {
                let key = (thread.to_owned(), turn.to_owned(), id.clone());
                if order.contains_key(&key) {
                    continue;
                }
                let position = order
                    .iter()
                    .filter(|((t, u, _), _)| t == thread && u == turn)
                    .map(|(_, p)| *p)
                    .max()
                    .map(|p| p.checked_add(ITEM_POSITION_STEP))
                    .unwrap_or(Some(FIRST_ITEM_POSITION))
                    .ok_or(AgentError::HistoryCursorLost)?;
                order.insert(key, position);
            }
        }
        let mut rows = Vec::new();
        for (item, id) in items.iter().zip(ids) {
            let position = order[&(thread.to_owned(), turn.to_owned(), id)];
            rows.extend(self.project_item(thread, turn, item, 0, position)?);
        }
        let fresh_ids = rows.iter().map(|item| item.id.clone()).collect();
        let replace_groups = rows
            .iter()
            .filter(|item| {
                matches!(&item.part, AgentPart::Tool(call)
                    if call.metadata.as_ref().is_some_and(|m| m["type"] == "collabAgentToolCall"))
            })
            .map(|item| item.message_id.clone())
            .collect();
        Ok(PreparedTimeline {
            items: rows,
            commit: Some(Box::new(move || {
                let mut accepted =
                    self.accept_order_candidate(thread, (order, epoch), fresh_ids, None)?;
                accepted.replace_groups = replace_groups;
                Ok(accepted)
            })),
        })
    }

    pub(crate) fn ordered_items(
        &self,
        thread: &str,
        turn: &str,
        item: &Value,
        at_ms: u64,
    ) -> Result<Vec<TimelineItem>, AgentError> {
        let position = self.item_position(thread, turn, &mapper::string(item, "id")?)?;
        self.project_item(thread, turn, item, at_ms, position)
    }

    fn project_item(
        &self,
        thread: &str,
        turn: &str,
        item: &Value,
        at_ms: u64,
        position: u64,
    ) -> Result<Vec<TimelineItem>, AgentError> {
        // A reasoning item past the part limit is kept to its first parts
        // rather than costing the connection.
        let truncated;
        let item = if item["type"] == "reasoning"
            && ["summary", "content"].iter().any(|key| {
                item[*key]
                    .as_array()
                    .is_some_and(|parts| parts.len() > super::MAX_REASONING_PARTS)
            }) {
            let mut clipped = item.clone();
            for key in ["summary", "content"] {
                if let Some(parts) = clipped[key].as_array_mut() {
                    parts.truncate(super::MAX_REASONING_PARTS);
                }
            }
            tracing::warn!(
                thread,
                "Codex reasoning item past the part limit; kept its first parts"
            );
            truncated = clipped;
            &truncated
        } else {
            item
        };
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
        self.clear_async_questions(id).await;
        self.loaded.lock().await.remove(id);
        self.next_models
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        self.metadata_versions.lock().await.remove(id);
        let _history_read = self.history.lock().await;
        let retained = self
            .history_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pages
            .contains_key(id);
        // Never hold history_state while acquiring order: acceptance locks in
        // the opposite order. Native unload keeps retained history anchors.
        if !retained {
            self.order
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(thread, _, _), _| thread != id);
            self.order_epochs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(id);
        }
    }

    /// Opening a selected timeline may rejoin a loaded shared runtime, or
    /// restore one with durable native-action evidence. This is not discovery:
    /// unknown unloaded threads stay untouched, and only official resume can
    /// grant ownership (including rejecting an external writer's OS lock).
    async fn rejoin_loaded(
        &self,
        id: &str,
        metadata: &Value,
        recover_questions: bool,
    ) -> Result<(), AgentError> {
        if !self.client.is_shared() || !self.can_rejoin_selected(id, metadata).await {
            return Ok(());
        }
        let _guard = self.mutations.lock().await;
        if self.client.is_subscribed(id) {
            drop(_guard);
            if recover_questions {
                self.schedule_question_recovery(id, metadata["historyMode"].as_str())
                    .await;
            }
            return Ok(());
        }
        // Recheck after waiting for another mutation, including provenance.
        // A remembered origin is permission to request resume, not ownership.
        let fresh = self.read_thread(id).await?;
        if !self.can_rejoin_selected(id, &fresh).await {
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
        // Recovery reads must not hold the mutation lock: a user's interrupt or
        // reply can proceed, and turn/subscription fences discard a stale read.
        drop(_guard);
        if recover_questions {
            self.schedule_question_recovery(id, metadata["historyMode"].as_str())
                .await;
        }
        Ok(())
    }

    /// Optional question recovery must never delay or reject opening a timeline.
    /// One read per thread, with bounded concurrency and the same ownership fence
    /// across scheduling, pagination, and event-pump publication.
    async fn schedule_question_recovery(&self, id: &str, history_mode: Option<&str>) {
        let Some(turn) = self.client.active_turn(id) else {
            return;
        };
        let Some(ownership) = self.client.subscription_revision(id) else {
            return;
        };
        let loaded = self.loaded.lock().await;
        if !self.client.is_shared() || !loaded.contains(id) {
            return;
        }
        let mut queue = self.question_recoveries.lock().await;
        // At most one valid pending request per loaded thread; stale requests
        // cannot accumulate as sessions close or change turns. Loaded threads
        // are already bounded by MAX_SESSIONS, plus at most four running reads.
        let mut stale = Vec::new();
        queue.pending.retain(|request| {
            let key = &request.key;
            let valid = loaded.contains(&key.thread)
                && self.client.subscription_revision(&key.thread) == Some(key.ownership)
                && self.client.active_turn(&key.thread).as_deref() == Some(key.turn.as_str());
            if !valid {
                stale.push(key.clone());
            }
            valid
        });
        for key in stale {
            queue.scheduled.remove(&key);
        }
        let key = QuestionRecoveryKey {
            thread: id.into(),
            turn,
            ownership,
        };
        if queue.scheduled.contains(&key) {
            return;
        }
        // Keep a single queued generation even if notifications advanced this
        // thread while stale ownership checks above were being evaluated.
        let mut superseded = Vec::new();
        queue.pending.retain(|request| {
            if request.key.thread == id {
                superseded.push(request.key.clone());
                false
            } else {
                true
            }
        });
        for key in superseded {
            queue.scheduled.remove(&key);
        }
        queue.scheduled.insert(key.clone());
        queue.pending.push_back(QuestionRecovery {
            key,
            history_mode: history_mode.map(str::to_owned),
        });
        if queue.workers >= 4 {
            return;
        }
        queue.workers += 1;
        drop(queue);
        drop(loaded);
        let client = self.client.clone();
        let loaded = self.loaded.clone();
        let queue = self.question_recoveries.clone();
        tokio::spawn(async move {
            loop {
                let request = {
                    let mut state = queue.lock().await;
                    let Some(request) = state.pending.pop_front() else {
                        state.workers -= 1;
                        return;
                    };
                    request
                };
                let key = &request.key;
                // Include connection establishment and initialization in the
                // attempt deadline, not just the subsequent page calls.
                tokio::select! {
                    _ = client.cancel_token().cancelled_owned() => {},
                    result = tokio::time::timeout(std::time::Duration::from_secs(60), Self::recover_async_questions(
                        &client, &loaded, &key.thread, &key.turn, key.ownership, request.history_mode.as_deref()
                    )) => {
                        if !matches!(result, Ok(Ok(()))) {
                            tracing::debug!("Codex optional question recovery incomplete; timeline remains available");
                        }
                    }
                }
                let mut state = queue.lock().await;
                state.scheduled.remove(key);
                if client.cancel_token().is_cancelled() {
                    state.pending.clear();
                    state.scheduled.clear();
                    state.workers -= 1;
                    return;
                }
            }
        });
    }

    /// Never infer callbacks from persisted/unowned history. This is a bounded
    /// canonical read of one currently owned shared turn, including its committed
    /// answers. Publish only after reaching verified end, in event-pump order.
    async fn recover_async_questions(
        client: &CodexClient,
        loaded: &Mutex<HashSet<String>>,
        id: &str,
        turn: &str,
        ownership: u64,
        history_mode: Option<&str>,
    ) -> Result<(), AgentError> {
        if client.subscription_revision(id) != Some(ownership)
            || client.active_turn(id).as_deref() != Some(turn)
            || !loaded.lock().await.contains(id)
        {
            return Ok(());
        }
        let mut budget = Budget::question_recovery();
        let reader = client.question_history_reader().await?;
        let mut cursor = None;
        let mut seen = HashSet::new();
        let mut items = vec![];
        loop {
            if client.subscription_revision(id) != Some(ownership)
                || client.active_turn(id).as_deref() != Some(turn)
            {
                return Ok(());
            }
            let (page, max_rows) = match history_mode {
                Some("paginated") => (budget.call(&reader, "thread/items/list", json!({
                    "threadId":id,"turnId":turn,"limit":100,"sortDirection":"asc","cursor":cursor
                })).await?, 100),
                Some("legacy") => {
                    // Legacy storage rejects thread/items/list. The official
                    // one-turn page merges its active in-memory canonical items.
                    let page = budget.call(&reader, "thread/turns/list", json!({
                        "threadId":id,"limit":1,"sortDirection":"desc","itemsView":"full"
                    })).await?;
                    let turns = page["data"].as_array().filter(|rows| rows.len() <= 1)
                        .ok_or_else(|| AgentError::Protocol("invalid Codex current-turn page".into()))?;
                    let Some(current) = turns.first().filter(|row| row["id"] == turn && row["status"] == "inProgress") else {
                        return Ok(());
                    };
                    if current["itemsView"] != "full" {
                        return Err(AgentError::Protocol("incomplete Codex current-turn items".into()));
                    }
                    let items = current["items"].as_array()
                        .ok_or_else(|| AgentError::Protocol("invalid Codex current-turn items".into()))?;
                    (json!({"data":items.iter().map(|item| json!({"turnId":turn,"item":item})).collect::<Vec<_>>(),"nextCursor":null}), items.len())
                },
                _ => return unsupported("Codex question recovery storage mode"),
            };
            let rows = page["data"]
                .as_array()
                .filter(|rows| rows.len() <= max_rows)
                .ok_or_else(|| {
                    AgentError::Protocol("invalid Codex question recovery page".into())
                })?;
            for row in rows {
                if row["turnId"] != turn || row["item"]["id"].as_str().is_none() {
                    return Err(AgentError::Protocol(
                        "Codex question recovery identity mismatch".into(),
                    ));
                }
                let item = &row["item"];
                if item["type"] == "userMessage"
                    || (item["type"] == "agentMessage" && item["questions"].is_array())
                {
                    items.push(item.clone());
                }
            }
            cursor = next_cursor(&page, &mut seen)?;
            if cursor.is_none() {
                if client.subscription_revision(id) == Some(ownership)
                    && client.active_turn(id).as_deref() == Some(turn)
                    && loaded.lock().await.contains(id)
                {
                    client.recovered_questions(ownership, id, turn, items)?;
                }
                return Ok(());
            }
        }
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

    /// Best-effort `skills/list`. Failure leaves `catalog.skills` empty and
    /// does not fail the model catalog that was already read.
    async fn attach_skills(&self, catalog: &mut AgentCatalog, directory: Option<&str>) {
        let mut params = json!({});
        if let Some(directory) = directory.filter(|directory| !directory.is_empty()) {
            params["cwds"] = json!([directory]);
        }
        let response = match Budget::new()
            .call(&self.client, "skills/list", params)
            .await
        {
            Ok(response) => response,
            Err(_) => return,
        };
        if let Ok(skills) = mapper::map_skills(&response) {
            catalog.skills = skills;
            self.store_skill_paths(&response);
            self.skills_catalog
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Keep the file path Codex needs to start a skill. An enabled copy wins
    /// over a disabled one with the same name.
    fn store_skill_paths(&self, response: &Value) {
        let mut next: HashMap<String, (String, bool)> = HashMap::new();
        let Some(groups) = response.get("data").and_then(Value::as_array) else {
            return;
        };
        for group in groups {
            let Some(rows) = group.get("skills").and_then(Value::as_array) else {
                continue;
            };
            for row in rows {
                let Some(name) = row.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(path) = row.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if !skill_name_ok(name) || !skill_path_ok(path) {
                    continue;
                }
                let enabled = row.get("enabled").and_then(Value::as_bool).unwrap_or(false);
                let key = name.to_ascii_lowercase();
                match next.get(&key) {
                    Some((_, true)) => {}
                    _ => {
                        next.insert(key, (path.to_string(), enabled));
                    }
                }
            }
        }
        if let Ok(mut paths) = self.skill_paths.lock() {
            *paths = next
                .into_iter()
                .map(|(name, (path, _))| (name, path))
                .collect();
        }
    }

    async fn skill_path(&self, session_id: &str, name: &str) -> Result<String, AgentError> {
        let thread = self.read_thread(session_id).await?;
        let mut params = json!({});
        if let Some(cwd) = thread["cwd"].as_str().filter(|cwd| !cwd.is_empty()) {
            params["cwds"] = json!([cwd]);
        }
        if let Ok(response) = Budget::new()
            .call(&self.client, "skills/list", params)
            .await
        {
            self.store_skill_paths(&response);
        }
        let key = name.to_ascii_lowercase();
        self.skill_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
            .ok_or_else(|| AgentError::InvalidRequest(format!("unknown Codex skill {name}")))
    }

    /// Start a turn whose input is already a Codex `UserInput` list.
    async fn submit_turn(&self, id: &str, input: Vec<Value>) -> Result<(), AgentError> {
        self.prepare_shared_interaction(id).await?;
        let _guard = self.mutations.lock().await;
        self.start_turn_locked(id, input)
            .await
            .map_err(SendFailure::into_error)
    }

    /// Caller holds `mutations`. Everything before `turn/start` is written
    /// fails as `NotSent`.
    async fn start_turn_locked(&self, id: &str, input: Vec<Value>) -> Result<(), SendFailure> {
        self.prepare_turn_locked(id)
            .await
            .map_err(SendFailure::NotSent)?;
        let mut params = json!({"threadId":id,"input":input});
        let model = self
            .next_models
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .filter(|choice| choice.accepted_revision.is_none())
            .map(|choice| choice.model.clone());
        if let Some(model) = model {
            let thread = self.read_thread(id).await.map_err(SendFailure::NotSent)?;
            if thread["modelProvider"] != model.provider_id {
                return Err(SendFailure::NotSent(AgentError::InvalidRequest(
                    "Codex thread provider changed; select a matching configured model".into(),
                )));
            }
            params["model"] = json!(model.model_id);
            if let Some(effort) = model.variant {
                params["effort"] = json!(effort);
            }
        }
        let revision = self
            .metadata_revision(id)
            .await
            .map_err(SendFailure::NotSent)?;
        if !self.client.is_ready() {
            return Err(SendFailure::NotSent(AgentError::NotAvailable(
                "Codex is not connected; nothing was sent".into(),
            )));
        }
        let response = self
            .client
            .call("turn/start", params)
            .await
            .map_err(SendFailure::after_write)?;
        mapper::string(&response["turn"], "id").map_err(SendFailure::Unknown)?;
        self.record_native_acceptance(id).await;
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
        self.spawn_metadata_refresh(id, revision);
        Ok(())
    }

    /// Ownership and idleness checks, and resuming an unloaded thread.
    async fn prepare_turn_locked(&self, id: &str) -> Result<(), AgentError> {
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
        Ok(())
    }

    fn commit_history(
        history: &mut HashMap<String, HistoryPage>,
        id: &str,
        mut page: HistoryPage,
    ) -> Result<(), AgentError> {
        page.retained_bytes = serde_json::to_vec(&(&page.pending, &page.remaining, &page.seen))
            .map_err(|_| AgentError::Protocol("invalid Codex history state".into()))?
            .len();
        if page.retained_bytes > MAX_HISTORY_RETAINED_BYTES {
            return Err(AgentError::RequestFailed(
                "Codex history retention budget exhausted; no page consumed".into(),
            ));
        }
        // The budget is shared by every session. Rather than refuse this one
        // because others hold pages, the least recently read of the others
        // give theirs up: their next earlier pull answers `HistoryCursorLost`
        // and the app re-reads the snapshot.
        let over_budget = |history: &HashMap<String, HistoryPage>| {
            let bytes = history
                .iter()
                .filter(|(key, _)| key.as_str() != id)
                .map(|(_, held)| held.retained_bytes)
                .sum::<usize>();
            (!history.contains_key(id) && history.len() >= MAX_SESSIONS)
                || bytes.saturating_add(page.retained_bytes) > MAX_HISTORY_RETAINED_BYTES
        };
        while over_budget(history) {
            let Some(oldest) = history
                .iter()
                .filter(|(key, _)| key.as_str() != id)
                .min_by_key(|(_, held)| held.read_at_ms)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            history.remove(&oldest);
        }
        page.read_at_ms = crate::tasks::domain::now_ms();
        history.insert(id.to_string(), page);
        Ok(())
    }

    fn accept_history(
        &self,
        id: &str,
        expected: Option<u64>,
        mut page: HistoryPage,
    ) -> Result<(), AgentError> {
        let mut history = self.history_state.lock().unwrap_or_else(|p| p.into_inner());
        if history.pages.get(id).map(|held| held.revision) != expected {
            return Err(AgentError::NotAvailable(
                "Codex history changed before snapshot acceptance; refresh again".into(),
            ));
        }
        // One process-wide monotonic token prevents deletion/recreation ABA.
        // It is not an upstream replay sequence or a presentation sort key.
        let revision = history
            .revision
            .checked_add(1)
            .ok_or_else(|| AgentError::NotAvailable("Codex history revision exhausted".into()))?;
        page.revision = revision;
        Self::commit_history(&mut history.pages, id, page)?;
        history.revision = revision;
        Ok(())
    }

    fn order_candidate(&self, thread: &str) -> (ItemOrder, u64) {
        let accepted = self.order.lock().unwrap_or_else(|p| p.into_inner());
        let epoch = self
            .order_epochs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(thread)
            .copied()
            .unwrap_or(0);
        (
            accepted
                .iter()
                .filter(|((t, _, _), _)| t == thread)
                .map(|(key, value)| (key.clone(), *value))
                .collect(),
            epoch,
        )
    }

    fn rebase_prepared_order(
        thread: &str,
        turn: &str,
        ids: &[String],
        order: &mut ItemOrder,
    ) -> Result<HashMap<String, String>, AgentError> {
        let unique: HashSet<_> = ids.iter().collect();
        if unique.len() != ids.len() {
            return Err(AgentError::Protocol(
                "duplicate Codex canonical item identity".into(),
            ));
        }
        let key = |id: &str| (thread.to_string(), turn.to_string(), id.to_string());
        let new_count = ids
            .iter()
            .filter(|id| !order.contains_key(&key(id)))
            .count();
        if order.len() + new_count > 8192 {
            return Err(AgentError::RequestFailed(
                "Codex history item-order capacity exhausted; page not consumed".into(),
            ));
        }
        let mut extra: Vec<_> = order
            .iter()
            .filter(|((t, u, id), _)| t == thread && u == turn && !unique.contains(id))
            .map(|((_, _, id), position)| (id.clone(), *position))
            .collect();
        extra.sort_by_key(|(_, position)| *position);
        let all = ids
            .iter()
            .cloned()
            .chain(extra.into_iter().map(|(id, _)| id));
        let mut changes = HashMap::new();
        for (index, id) in all.enumerate() {
            let position = FIRST_ITEM_POSITION + (index as u64) * ITEM_POSITION_STEP;
            if let Some(old) = order
                .insert(key(&id), position)
                .filter(|old| *old != position)
            {
                changes.insert(
                    mapper::order_group(thread, turn, old),
                    mapper::order_group(thread, turn, position),
                );
            }
        }
        Ok(changes)
    }

    async fn prepare_history_order(
        &self,
        thread: &str,
        turn: &str,
        ids: &[String],
        prefix: bool,
        order: &mut ItemOrder,
        budget: &mut Budget,
    ) -> Result<HashMap<String, String>, AgentError> {
        match Self::assign_history_order(order, thread, turn, ids, prefix) {
            Ok(()) => return Ok(HashMap::new()),
            Err(OrderError::RebaseRequired) => {}
            Err(OrderError::Invalid(error)) => return Err(error),
        }
        // Repair only the affected turn. Nothing accepted is mutated while
        // fetching, and the ordinary page cursor remains unconsumed on failure.
        let mut canonical = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let response = budget.call(&self.client,"thread/items/list",json!({"threadId":thread,"turnId":turn,"sortDirection":"asc","limit":100,"cursor":cursor})).await?;
            let rows = response["data"]
                .as_array()
                .ok_or_else(|| AgentError::Protocol("invalid Codex canonical turn page".into()))?;
            if rows.len() > 100 {
                return Err(AgentError::Protocol(
                    "oversized Codex canonical turn page".into(),
                ));
            }
            budget.rows(rows.len())?;
            for row in rows {
                if row["turnId"] != turn {
                    return Err(AgentError::Protocol(
                        "Codex canonical turn identity mismatch".into(),
                    ));
                }
                canonical.push(mapper::string(&row["item"], "id")?);
            }
            if canonical.len() > 8192 {
                return Err(AgentError::RequestFailed(
                    "Codex canonical turn item limit exceeded".into(),
                ));
            }
            cursor = next_cursor(&response, &mut seen)?;
            if cursor.is_none() {
                break;
            }
            if rows.is_empty() {
                return Err(AgentError::Protocol(
                    "empty continuing Codex canonical turn page".into(),
                ));
            }
        }
        let available: HashSet<_> = canonical.iter().collect();
        if ids.iter().any(|id| !available.contains(id)) {
            return Err(AgentError::HistoryCursorLost);
        }
        Self::rebase_prepared_order(thread, turn, &canonical, order)
    }

    fn accept_history_order(
        &self,
        thread: &str,
        expected: Option<u64>,
        page: HistoryPage,
        candidate: (ItemOrder, u64),
        fresh_ids: HashSet<String>,
    ) -> Result<HistoryCommit, AgentError> {
        self.accept_order_candidate(thread, candidate, fresh_ids, Some((expected, page)))
    }

    fn accept_order_candidate(
        &self,
        thread: &str,
        (mut candidate, expected_epoch): (ItemOrder, u64),
        fresh_ids: HashSet<String>,
        history: Option<(Option<u64>, HistoryPage)>,
    ) -> Result<HistoryCommit, AgentError> {
        let mut accepted = self.order.lock().unwrap_or_else(|p| p.into_inner());
        let mut epochs = self.order_epochs.lock().unwrap_or_else(|p| p.into_inner());
        if epochs.get(thread).copied().unwrap_or(0) != expected_epoch {
            return Err(AgentError::HistoryCursorLost);
        }
        if epochs.len() >= MAX_SESSIONS && !epochs.contains_key(thread) {
            return Err(AgentError::RequestFailed(
                "Codex ordering session retention exhausted".into(),
            ));
        }
        let next_epoch = expected_epoch
            .checked_add(1)
            .ok_or_else(|| AgentError::NotAvailable("Codex ordering revision exhausted".into()))?;
        let new_count = candidate
            .keys()
            .filter(|key| !accepted.contains_key(*key))
            .count();
        if accepted.len() + new_count > 8192 {
            return Err(AgentError::RequestFailed(
                "Codex history item-order capacity exhausted; page not consumed".into(),
            ));
        }
        // Live observations after the read started are retained, never deleted
        // by a historical snapshot. Their provisional relative order is kept.
        let mut live: Vec<_> = accepted
            .iter()
            .filter(|((t, _, _), _)| t == thread)
            .filter(|(key, _)| !candidate.contains_key(*key))
            .map(|(key, p)| (key.clone(), *p))
            .collect();
        live.sort_by_key(|(key, p)| (key.1.clone(), *p));
        for (key, _) in live {
            let position = candidate
                .iter()
                .filter(|((t, u, _), _)| t == &key.0 && u == &key.1)
                .map(|(_, p)| *p)
                .max()
                .map(|p| p.checked_add(ITEM_POSITION_STEP))
                .unwrap_or(Some(FIRST_ITEM_POSITION))
                .ok_or_else(|| {
                    AgentError::RequestFailed("Codex live order capacity exhausted".into())
                })?;
            candidate.insert(key, position);
        }
        let mut changes = HashMap::new();
        for ((t, u, id), new) in &candidate {
            if let Some(old) = accepted
                .get(&(t.clone(), u.clone(), id.clone()))
                .filter(|old| *old != new)
            {
                changes.insert(
                    mapper::order_group(t, u, *old),
                    mapper::order_group(t, u, *new),
                );
            }
        }
        // This hook is invoked under the mirror write lock. After cursor
        // validation there are no awaits or fallible operations before publish.
        if let Some((expected, page)) = history {
            self.accept_history(thread, expected, page)?;
        } else {
            // Prepared older rows must use the same projection after a live
            // canonical completion. In-flight candidates are fenced by epoch.
            let mut retained = self.history_state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(page) = retained.pages.get_mut(thread) {
                for item in &mut page.remaining {
                    if let Some(next) = changes.get(&item.message_id) {
                        item.message_id.clone_from(next);
                    }
                }
            }
        }
        accepted.extend(candidate);
        epochs.insert(thread.to_owned(), next_epoch);
        Ok(HistoryCommit {
            order: changes,
            fresh_ids,
            replace_groups: Default::default(),
        })
    }

    /// Legacy storage has turn-level cursors, not item cursors. Keep only
    /// the requested turn and its mapped residual, never hydrate the thread.
    async fn read_legacy_turn_page(
        &self,
        id: &str,
        state: &mut HistoryPage,
        budget: &mut Budget,
        order: &mut ItemOrder,
    ) -> Result<(), AgentError> {
        let response = budget.call(&self.client, "thread/turns/list", json!({
            "threadId":id,"limit":1,"sortDirection":"desc","itemsView":"full","cursor":state.cursor
        })).await?;
        let turns = response["data"]
            .as_array()
            .filter(|rows| rows.len() <= 1)
            .ok_or_else(|| AgentError::Protocol("invalid Codex legacy turn page".into()))?;
        let next = next_cursor(&response, &mut state.seen)?;
        if turns.is_empty() && next.is_some() {
            return Err(AgentError::Protocol(
                "empty continuing Codex turn page".into(),
            ));
        }
        for turn in turns {
            let turn_id = mapper::string(turn, "id")?;
            if turn["itemsView"] != "full" {
                return Err(AgentError::Protocol("incomplete Codex legacy turn".into()));
            }
            let rows = turn["items"]
                .as_array()
                .ok_or_else(|| AgentError::Protocol("invalid Codex legacy turn items".into()))?;
            let ids = rows
                .iter()
                .map(|row| mapper::string(row, "id"))
                .collect::<Result<Vec<_>, _>>()?;
            Self::rebase_prepared_order(id, &turn_id, &ids, order)?;
            for row in rows {
                state.pending.push(json!({"turnId":turn_id,"item":row,
                    "startedAtMs":turn["startedAt"].as_u64().unwrap_or(0).saturating_mul(1000)}));
            }
        }
        state.cursor = next;
        state.started = true;
        Ok(())
    }

    async fn read_history_batch(
        &self,
        id: &str,
        limit: usize,
        state: &mut HistoryPage,
        budget: &mut Budget,
        order: &mut ItemOrder,
    ) -> Result<Vec<TimelineItem>, AgentError> {
        let mut items = Vec::new();
        while items.len() < limit {
            if !state.remaining.is_empty() {
                let start = state.remaining.len().saturating_sub(limit - items.len());
                items.extend(state.remaining.drain(start..));
                continue;
            }
            if state.pending.is_empty() {
                if state.started && state.cursor.is_none() {
                    break;
                }
                // A short, nonempty page is not EOF. Preserve the real cursor
                // and publish complete items rather than repeatedly rejecting
                // a large historical window at the next bounded read.
                if !items.is_empty()
                    && self.client.is_shared()
                    && !budget.can_read_complete_history_item()
                {
                    break;
                }
                if state.legacy_turns {
                    self.read_legacy_turn_page(id, state, budget, order).await?;
                    // Empty turns are not EOF; their canonical cursor decides.
                    if state.pending.is_empty() {
                        continue;
                    }
                } else {
                    let page_limit = (limit - items.len()).min(100);
                    let mut page = budget.call(&self.client, "thread/items/list", json!({
                    "threadId":id,"limit":page_limit,"sortDirection":"desc","cursor":state.cursor
                })).await?;
                    let data = page["data"]
                        .as_array()
                        .ok_or_else(|| AgentError::Protocol("invalid Codex item page".into()))?;
                    if data.len() > page_limit {
                        return Err(AgentError::Protocol("oversized Codex item page".into()));
                    }
                    budget.rows(data.len())?;
                    let next = next_cursor(&page, &mut state.seen)?;
                    if state.seen.len() > 8192 {
                        return Err(AgentError::RequestFailed(
                            "Codex history cursor capacity exhausted".into(),
                        ));
                    }
                    if data.is_empty() && next.is_some() {
                        return Err(AgentError::Protocol(
                            "empty continuing Codex item page".into(),
                        ));
                    }
                    let mut ids_by_turn: HashMap<String, Vec<String>> = HashMap::new();
                    for row in data.iter().rev() {
                        let turn = mapper::string(row, "turnId")?;
                        mapper::string(&row["item"], "type")?;
                        if row["item"]["type"] == "reasoning"
                            && ["summary", "content"].iter().any(|key| {
                                row["item"][*key]
                                    .as_array()
                                    .is_some_and(|parts| parts.len() > super::MAX_REASONING_PARTS)
                            })
                        {
                            return Err(AgentError::Protocol(
                                "Codex reasoning part limit exceeded".into(),
                            ));
                        }
                        ids_by_turn
                            .entry(turn)
                            .or_default()
                            .push(mapper::string(&row["item"], "id")?);
                    }
                    for (turn, ids) in ids_by_turn {
                        let changes = self
                            .prepare_history_order(id, &turn, &ids, state.started, order, budget)
                            .await?;
                        for row in state.remaining.iter_mut().chain(items.iter_mut()) {
                            if let Some(next) = changes.get(&row.message_id) {
                                row.message_id.clone_from(next);
                            }
                        }
                    }
                    state.pending = page["data"]
                        .as_array_mut()
                        .expect("validated item page")
                        .drain(..)
                        .rev()
                        .collect();
                    state.cursor = next;
                    state.started = true;
                    if state.pending.is_empty() {
                        break;
                    }
                }
            }
            // Project the accepted page once and release all raw image bodies.
            // A row-limited window retains only the mapped older prefix, not a
            // giant item or the other raw images adjacent to it in this page.
            let pending = if self.client.is_shared() {
                std::mem::take(&mut state.pending)
            } else {
                vec![state.pending.pop().expect("nonempty history page")]
            };
            for row in pending {
                let turn = mapper::string(&row, "turnId")?;
                state.remaining.extend(
                    self.project_item(
                        id,
                        &turn,
                        &row["item"],
                        row["startedAtMs"]
                            .as_u64()
                            .or_else(|| row["completedAtMs"].as_u64())
                            .unwrap_or(0),
                        *order
                            .get(&(
                                id.to_string(),
                                turn.clone(),
                                mapper::string(&row["item"], "id")?,
                            ))
                            .ok_or_else(|| {
                                AgentError::Protocol("missing prepared Codex item order".into())
                            })?,
                    )?,
                );
            }
        }
        items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        Ok(items)
    }

    /// Local command catalog, not an app-server call. A missing file leaves
    /// the command list empty and `slash_commands` off.
    fn attach_commands(
        &self,
        catalog: &mut AgentCatalog,
        snapshot: Option<&crate::command_catalog::Snapshot>,
    ) {
        catalog.commands = snapshot
            .map(|snapshot| mapper::commands_from_catalog(&snapshot.commands))
            .unwrap_or_default();
        self.slash_catalog.store(
            !catalog.commands.is_empty(),
            std::sync::atomic::Ordering::Release,
        );
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
    fn account_limits(
        &self,
    ) -> AgentFuture<'_, crate::agents::domain::account_limits::AccountLimits> {
        Box::pin(async { Ok(self.account_limits.read(&self.client).await) })
    }

    fn prepare_terminal_launch<'a>(
        &'a self,
        directory: &'a str,
        title: &'a str,
        arguments: &'a [String],
    ) -> AgentFuture<'a, PreparedTerminalLaunch> {
        Box::pin(async move {
            let endpoint = self.client.terminal_endpoint().ok_or_else(|| {
                AgentError::Unsupported("a connected shared Codex daemon is required".into())
            })?;
            let config = super::launch::parse(directory, arguments)
                .map_err(|reason| AgentError::Unsupported(reason.into()))?;
            if title.trim().is_empty() {
                return Err(AgentError::InvalidRequest(
                    "shared Codex startup requires a task title".into(),
                ));
            }
            let _guard = self.mutations.lock().await;
            let response = self.client.call("thread/start", config.params).await?;
            let mut session = self.selected_response(response).await?;
            if uuid::Uuid::parse_str(&session.asid.0).is_err() {
                return Err(AgentError::Protocol(
                    "Codex returned an invalid terminal session identity".into(),
                ));
            }
            // New official threads persist lazily. Even an already-loaded
            // thread/resume reads its stored record, so a TUI cannot attach
            // until it exists. Naming the task materializes it through the
            // official metadata path without submitting a model turn.
            let named = self
                .client
                .call(
                    "thread/name/set",
                    json!({"threadId":session.asid.0,"name":title}),
                )
                .await?;
            if !named.is_object() {
                return Err(AgentError::Protocol(
                    "invalid Codex terminal session name acknowledgement".into(),
                ));
            }
            session.title = title.into();
            let mut arguments = vec!["--remote".into(), endpoint];
            arguments.extend(config.ui_args);
            arguments.extend(["resume".into(), session.asid.0.clone()]);
            Ok(PreparedTerminalLaunch { session, arguments })
        })
    }

    fn adds_mcp_servers(&self) -> bool {
        true
    }

    fn refresh_session_mcp<'a>(&'a self, asid: &'a str) -> AgentFuture<'a, bool> {
        Box::pin(async move {
            self.client
                .call("config/mcpServer/reload", Value::Null)
                .await?;
            super::mcp::connected(&self.client, Some(asid)).await
        })
    }

    fn session_mcp_connected<'a>(
        &'a self,
        asid: &'a str,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, bool> {
        Box::pin(async move { super::mcp::connected(&self.client, Some(asid)).await })
    }

    fn mcp_server_connected<'a>(
        &'a self,
        _name: &'a str,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, bool> {
        Box::pin(async move { super::mcp::connected(&self.client, None).await })
    }

    fn add_mcp_server<'a>(
        &'a self,
        name: &'a str,
        config: &'a Value,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if name != "muqun" {
                return unsupported("MCP server registration");
            }
            super::mcp::prepare(&self.client, config, directory).await
        })
    }

    fn supports_earlier_timeline(&self) -> bool {
        true
    }

    fn kind(&self) -> &'static str {
        super::KIND
    }
    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async { Ok(self.client.is_ready()) })
    }
    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::NativeUploads
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
            let session = self
                .selected_response(self.client.call("thread/start", params).await?)
                .await?;
            self.record_native_acceptance(&session.asid.0).await;
            Ok(session)
        })
    }

    fn session_runtime<'a>(&'a self, id: &'a str) -> AgentFuture<'a, Option<SessionRuntime>> {
        Box::pin(async move { Ok(self.runtime(id, false).await) })
    }

    fn prepare_selected_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if self.client.is_shared() && !self.client.is_subscribed(id) {
                let thread = self.read_thread(id).await?;
                self.rejoin_loaded(id, &thread, true).await?;
            }
            Ok(())
        })
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
            let delivery = match delivery {
                None => None,
                Some(mode) => Some(Delivery::parse(mode).ok_or_else(|| {
                    AgentError::InvalidRequest("delivery must be steer or queue".into())
                })?),
            };
            validate_id(id)?;
            if text.is_empty() && attachments.is_empty() {
                return Err(AgentError::InvalidRequest("empty Codex prompt".into()));
            }
            // Validate uploads before any rejoin/resume or prompt mutation.
            let prepared = super::input::prepare(text, attachments).await?;
            let input = prepared.input;
            // Compaction cannot consume steering yet. Publish the accepted
            // prompt through the ordinary inbox instead of hiding it upstream.
            if delivery == Some(Delivery::Queue) || self.client.is_compacting(id) {
                self.prepare_shared_interaction(id).await?;
                let _guard = self.mutations.lock().await;
                self.require_native_interaction(id).await?;
                if self.client.active_turn(id).is_none() {
                    // Idle: nothing to wait for, so it starts now, as an idle
                    // OpenCode session delivers a queued prompt at once.
                    return self
                        .start_turn_locked(id, input)
                        .await
                        .map_err(SendFailure::into_error);
                }
                if !self.loaded.lock().await.contains(id) {
                    return unsupported("Codex queued delivery requires an owned live thread");
                }
                return self.enqueue_prompt(
                    id,
                    text,
                    prepared.files,
                    input,
                    delivery.unwrap_or(Delivery::Steer),
                );
            }
            if delivery == Some(Delivery::Steer) {
                self.prepare_shared_interaction(id).await?;
                let _guard = self.mutations.lock().await;
                self.require_native_interaction(id).await?;
                if !self.loaded.lock().await.contains(id) {
                    return unsupported("Codex steering requires an owned live thread");
                }
                // Compaction may have begun while preparing the interaction.
                if self.client.is_compacting(id) {
                    return self.enqueue_prompt(id, text, prepared.files, input, Delivery::Steer);
                }
                let turn = self.client.active_turn(id).ok_or_else(|| {
                    AgentError::InvalidRequest(
                        "Codex active turn ended before steering; message was not resent".into(),
                    )
                })?;
                return self.steer(id, &turn, input).await;
            }
            self.submit_turn(id, input).await
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

    fn compact_session<'a>(
        &'a self,
        id: &'a str,
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, Value> {
        Box::pin(async move {
            validate_id(id)?;
            if delivery.is_some() {
                return unsupported("Codex queued or steered compaction");
            }
            let _guard = self.mutations.lock().await;
            // Compaction never adopts or resumes a historical thread. The
            // accepted start/resume/fork and current subscription prove ownership.
            let ownership = self.client.subscription_revision(id);
            if !self.loaded.lock().await.contains(id)
                || (self.client.is_shared() && ownership.is_none())
            {
                return Err(AgentError::InvalidRequest(
                    "Codex compaction requires an owned live session".into(),
                ));
            }
            self.require_native_interaction(id).await?;
            let thread = self.read_thread(id).await?;
            if !self.loaded.lock().await.contains(id)
                || (self.client.is_shared() && self.client.subscription_revision(id) != ownership)
            {
                return Err(AgentError::InvalidRequest(
                    "Codex session ownership changed during compaction check".into(),
                ));
            }
            if self.client.active_turn(id).is_some() || thread["status"]["type"] != "idle" {
                return Err(AgentError::InvalidRequest(
                    "Codex compaction requires an idle session".into(),
                ));
            }
            self.client
                .call("thread/compact/start", json!({"threadId":id}))
                .await
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
            if let (Some(sessions), Some(home)) = (&self.native_sessions, self.client.codex_home())
            {
                if sessions.remove(home, id).await.is_err() {
                    tracing::warn!("Could not remove deleted native Codex session evidence");
                }
            }
            let _history_read = self.history.lock().await;
            self.history_state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pages
                .remove(id);
            drop(_history_read);
            self.forget_thread(id).await;
            self.clear_usage(id).await;
            self.clear_inbox(id);
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
            let session = self
                .selected_response(
                    self.client
                        .call("thread/fork", json!({"threadId":id,"excludeTurns":true}))
                        .await?,
                )
                .await?;
            self.record_native_acceptance(&session.asid.0).await;
            Ok(session)
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
            if id.starts_with("async:") {
                let _guard = self.mutations.lock().await;
                self.require_native_interaction(thread).await?;
                if !self.client.is_ready() || !self.loaded.lock().await.contains(thread) {
                    return unsupported("Codex asynchronous reply requires an owned live thread");
                }
                let turn = self.client.active_turn(thread).ok_or_else(|| {
                    AgentError::InvalidRequest("Codex asynchronous question turn ended".into())
                })?;
                let text = {
                    let mut questions = self.async_questions.lock().await;
                    questions
                        .retain_subscription(thread, self.client.subscription_revision(thread));
                    questions.claim(thread, &turn, id, &answers)?
                };
                if let Err(error) = self
                    .steer(thread, &turn, super::input::text_input(&text))
                    .await
                {
                    // An explicit RPC rejection did not accept this input.
                    // Transport/cancellation errors keep the claim fenced.
                    if matches!(error, AgentError::RequestFailed(_)) {
                        self.async_questions.lock().await.rejected(id);
                    }
                    return Err(error);
                }
                self.async_questions.lock().await.accept(id);
                return self.client.resolve_form(thread, id);
            }
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
            let mut forms: Vec<_> = self
                .client
                .server_requests(id)
                .iter()
                .filter_map(|r| self.form(&r.id, &r.method, &r.params))
                .collect();
            if self.client.is_ready() {
                let mut questions = self.async_questions.lock().await;
                questions.retain_subscription(id, self.client.subscription_revision(id));
                forms.extend(questions.pending(id, self.client.active_turn(id).as_deref()));
            }
            Ok(forms)
        })
    }

    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            self.skills_catalog
                .store(false, std::sync::atomic::Ordering::Release);
            self.slash_catalog
                .store(false, std::sync::atomic::Ordering::Release);
            let mut catalog = self.catalog().await?;
            self.attach_skills(&mut catalog, directory).await;
            self.attach_commands(&mut catalog, crate::command_catalog::load("codex").as_ref());
            Ok(catalog)
        })
    }

    fn skills_listed(&self) -> bool {
        self.skills_catalog
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn slash_commands_listed(&self) -> bool {
        self.slash_catalog
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn activate_skill<'a>(
        &'a self,
        session_id: &'a str,
        name: &'a str,
        resume: Option<bool>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if resume == Some(true) {
                return unsupported("Codex skill resume");
            }
            validate_id(session_id)?;
            let name = name.trim();
            if !skill_name_ok(name) {
                return Err(AgentError::InvalidRequest(
                    "invalid Codex skill name".into(),
                ));
            }
            let path = self.skill_path(session_id, name).await?;
            self.submit_turn(
                session_id,
                vec![json!({"type":"skill","name":name,"path":path})],
            )
            .await
        })
    }

    fn load_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let prepared = self.prepare_earlier_timeline(session_id, limit).await?;
            if let Some(commit) = prepared.commit {
                commit()?;
            }
            Ok(prepared.items)
        })
    }

    fn prepare_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, PreparedTimeline<'a>> {
        Box::pin(async move {
            if limit == 0 || limit > MAX_HISTORY_ITEMS {
                return Err(AgentError::InvalidRequest(
                    "Codex history limit exceeds 2000 mapped rows".into(),
                ));
            }
            validate_id(session_id)?;
            let _history_read = self.history.lock().await;
            let Some(mut page) = self
                .history_state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pages
                .get(session_id)
                .cloned()
            else {
                return Err(AgentError::HistoryCursorLost);
            };
            if page.started
                && page.cursor.is_none()
                && page.pending.is_empty()
                && page.remaining.is_empty()
            {
                return Ok(PreparedTimeline::ready(Vec::new()));
            }
            let expected = Some(page.revision);
            let mut budget = Budget::new();
            let (mut order, epoch) = self.order_candidate(session_id);
            let items = self
                .read_history_batch(session_id, limit, &mut page, &mut budget, &mut order)
                .await?;
            let fresh_ids = items.iter().map(|item| item.id.clone()).collect();
            Ok(PreparedTimeline {
                items,
                commit: Some(Box::new(move || {
                    self.accept_history_order(session_id, expected, page, (order, epoch), fresh_ids)
                })),
            })
        })
    }

    fn run_command<'a>(
        &'a self,
        session_id: &'a str,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // TUI commands and custom prompt names are not app-server RPCs.
            // Never turn an unsupported command into a billable model turn.
            if !mapper::is_compact_command(name) {
                return unsupported("Codex slash command");
            }
            if !arguments.trim().is_empty() {
                return Err(AgentError::InvalidRequest(
                    "Codex compact takes no arguments".into(),
                ));
            }
            self.compact_session(session_id, delivery).await.map(|_| ())
        })
    }

    fn mcp_call_timeline<'a>(
        &'a self,
        id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Option<Vec<TimelineItem>>> {
        Box::pin(async move {
            let metadata = self.read_thread(id).await?;
            if metadata["status"]["type"] == "idle" {
                return Ok(None);
            }
            super::mcp::call_timeline(&self.client, id, limit, metadata["historyMode"].as_str())
                .await
                .map(Some)
        })
    }

    fn get_timeline<'a>(&'a self, id: &'a str, limit: usize) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let prepared = self.prepare_timeline(id, limit).await?;
            if let Some(commit) = prepared.commit {
                commit()?;
            }
            // The final commit and return are in one poll, with no cancellable
            // await after pagination publication (including the rollout path).
            Ok(prepared.items)
        })
    }

    fn prepare_timeline<'a>(
        &'a self,
        id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, PreparedTimeline<'a>> {
        Box::pin(async move {
            if limit == 0 {
                return Ok(PreparedTimeline::ready(vec![]));
            }
            if limit > MAX_HISTORY_ITEMS {
                return Err(AgentError::InvalidRequest(
                    "Codex history limit exceeds 2000 mapped rows".into(),
                ));
            }
            validate_id(id)?;
            // Prepared candidates live only in bounded caller-owned leases,
            // never an unbounded token/cache registry. Drop also releases a slot.
            let permit = self
                .history_preparations
                .clone()
                .try_acquire_owned()
                .map_err(|_| {
                    AgentError::NotAvailable("Codex history preparation capacity exhausted".into())
                })?;
            // Fence the whole preparation, including metadata/rollout I/O and
            // waiting for the page-read gate, not just the final item-page read.
            let expected = self
                .history_state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pages
                .get(id)
                .map(|held| held.revision);
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
            self.rejoin_loaded(id, thread, true).await?;
            let prepared = super::rollout::Rollouts::prepare(self, thread).await;
            // Rejoin uses the mutation lock; never take it while holding the
            // history lock (delete holds mutations before clearing history).
            let _history_read = self.history.lock().await;
            let mut page = HistoryPage::default();
            let (mut order, epoch) = self.order_candidate(id);
            page.legacy_turns = match thread["historyMode"].as_str() {
                Some("legacy") => true,
                Some("paginated") => false,
                _ => return unsupported("Codex thread history storage mode"),
            };
            let items = self
                .read_history_batch(id, limit, &mut page, &mut budget, &mut order)
                .await?;
            // Finish every async preparation before exposing the candidate.
            // A usage-lock wait, timeout or cancellation cannot reset history.
            if let Some(prepared) = &prepared {
                self.seed_usage(id, prepared.usage.as_ref(), prepared.external())
                    .await;
            }
            let fresh_ids = items.iter().map(|item| item.id.clone()).collect();
            Ok(PreparedTimeline {
                items,
                commit: Some(Box::new(move || {
                    let _permit = permit;
                    let accepted =
                        self.accept_history_order(id, expected, page, (order, epoch), fresh_ids)?;
                    if let Some(prepared) = prepared {
                        self.rollouts.install(id, prepared);
                    }
                    Ok(accepted)
                })),
            })
        })
    }

    fn get_inbox<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async move {
            validate_id(session_id)?;
            Ok(self.inbox_items(session_id))
        })
    }

    fn cancel_inbox_item<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            validate_id(session_id)?;
            // Waits for an in-flight delivery: a delivered item is not cancelled.
            let _guard = self.mutations.lock().await;
            self.queue()
                .remove(session_id, inbox_id)
                .ok_or_else(|| inbox_item_not_found(inbox_id))?;
            self.signal(InboxSignal::Changed(session_id.into()));
            Ok(())
        })
    }

    fn set_inbox_delivery<'a>(
        &'a self,
        session_id: &'a str,
        inbox_id: &'a str,
        delivery: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            validate_id(session_id)?;
            match Delivery::parse(delivery) {
                Some(Delivery::Steer) => self.send_queued_now(session_id, inbox_id).await,
                Some(Delivery::Queue) => {
                    let _guard = self.mutations.lock().await;
                    if !self
                        .queue()
                        .set_delivery(session_id, inbox_id, Delivery::Queue)
                    {
                        return Err(inbox_item_not_found(inbox_id));
                    }
                    self.signal(InboxSignal::Changed(session_id.into()));
                    Ok(())
                }
                None => Err(AgentError::InvalidRequest(
                    "delivery must be steer or queue".into(),
                )),
            }
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
        query: &'a str,
        limit: usize,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let root = directory
                .map(str::trim)
                .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
                .ok_or_else(|| {
                    AgentError::InvalidRequest(
                        "Codex file search needs an absolute directory".into(),
                    )
                })?;
            if limit == 0 || limit > 50 {
                return Err(AgentError::InvalidRequest(
                    "Codex file search limit must be 1 to 50".into(),
                ));
            }
            let query = query.trim();
            if query.len() > 256 {
                return Err(AgentError::InvalidRequest(
                    "Codex file search query is too long".into(),
                ));
            }
            // A bare `@`: Codex's fuzzy search matches nothing for an empty
            // query, so answer the way OpenCode's empty query does -- the
            // shallowest files of the session's directory -- with the same
            // fenced walk a pane's `@` uses.
            if query.is_empty() {
                let base = std::path::PathBuf::from(root);
                let hits = tokio::task::spawn_blocking(move || {
                    crate::composer::search_files(&base, "", limit)
                })
                .await
                .unwrap_or_default();
                return Ok(hits
                    .into_iter()
                    .map(|hit| format!("{}/{}", root.trim_end_matches('/'), hit.path))
                    .filter(|path| skill_path_ok(path))
                    .map(|path| json!({"path": path, "type": "file"}))
                    .collect());
            }
            let response = self
                .client
                .call("fuzzyFileSearch", json!({"query": query, "roots": [root]}))
                .await?;
            let mut files = response
                .get("files")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            files.sort_by(|a, b| {
                b.get("score")
                    .and_then(Value::as_u64)
                    .cmp(&a.get("score").and_then(Value::as_u64))
            });
            Ok(files
                .into_iter()
                .filter_map(|row| {
                    let relative = row.get("path").and_then(Value::as_str)?;
                    let path = if relative.starts_with('/') {
                        relative.to_string()
                    } else {
                        format!("{}/{}", root.trim_end_matches('/'), relative)
                    };
                    if !skill_path_ok(&path) {
                        return None;
                    }
                    let kind = match row.get("match_type").and_then(Value::as_str) {
                        Some("directory") => "directory",
                        _ => "file",
                    };
                    Some(json!({"path": path, "type": kind}))
                })
                .take(limit)
                .collect())
        })
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
mod history_tests {
    use super::super::tests::{golden, history_fixture_page, Fixture};
    use super::*;
    use crate::agents::adapters::memory_mirror::MemoryMirror;
    use crate::agents::ports::mirror::SessionMirrorPort;
    use crate::agents::use_cases::SessionService;
    use std::future::Future;
    use std::task::Poll;

    #[cfg(unix)]
    #[tokio::test]
    async fn shared_question_recovery_failure_does_not_reject_selection_or_publish_partial_forms() {
        use crate::agents::adapters::codex::client::websocket::tests::Fixture as SharedFixture;
        for scenario in ["wrong_turn", "cycle", "wire_budget"] {
            let page = std::sync::atomic::AtomicUsize::new(0);
            let mut fixture = SharedFixture::new(move |request| match request["method"].as_str().unwrap() {
                "thread/read" | "thread/resume" => json!({"thread":{"id":"live","historyMode":"paginated","status":{"type":"active","activeFlags":[]}}}),
                "thread/turns/list" => json!({"data":[{"id":"turn","status":"inProgress"}]}),
                "thread/items/list" if request["params"]["turnId"].is_null() => json!({"data":[],"nextCursor":null}),
                "thread/items/list" => match scenario {
                    "wrong_turn" => json!({"data":[{"turnId":"old-turn","item":{"id":"q","type":"agentMessage","questions":[{"title":"Old?"}]}}],"nextCursor":null}),
                    "cycle" => json!({"data":[],"nextCursor":"same"}),
                    _ => json!({"data":[{"turnId":"turn","item":{"id":"q","type":"agentMessage","questions":[{"title":"Unverified?"}],"text":"x".repeat(1024*1024)}}],"nextCursor":page.fetch_add(1,std::sync::atomic::Ordering::SeqCst).to_string()}),
                },
                other => panic!("unexpected RPC {other}"),
            }).await;
            let driver = CodexDriver::new(fixture.client.clone());
            driver.prepare_selected_session("live").await.unwrap();
            driver.get_timeline("live", 10).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !driver.question_recoveries.lock().await.scheduled.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            driver.get_session("live").await.unwrap();
            assert!(driver.get_pending_forms("live").await.unwrap().is_empty());
            while let Ok(event) = fixture.events.try_recv() {
                assert!(
                    !matches!(event, super::super::CodexEvent::RecoveredQuestions { .. }),
                    "{scenario}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shared_question_recovery_is_nonblocking_deduplicated_and_fenced_in_flight() {
        use super::super::CodexEvent;
        use crate::agents::adapters::codex::client::websocket::tests::Fixture as SharedFixture;
        for scenario in ["pending", "completed", "new_turn", "unloaded", "cancelled"] {
            let gate = Arc::new(tokio::sync::Notify::new());
            let mut fixture = SharedFixture::with_deferred(|request| match request["method"].as_str().unwrap() {
                "thread/read" | "thread/resume" => json!({"thread":{"id":"live","historyMode":"paginated","status":{"type":"active","activeFlags":[]}}}),
                "thread/turns/list" => json!({"data":[{"id":"turn","status":"inProgress"}]}),
                "thread/items/list" => json!({"data":[{"turnId":"turn","item":{"id":"q","type":"agentMessage","questions":[{"title":"Pending?"}]}}],"nextCursor":null}),
                other => panic!("unexpected RPC {other}"),
            }, Some(("thread/items/list", gate.clone()))).await;
            let driver = CodexDriver::new(fixture.client.clone());
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                driver.prepare_selected_session("live"),
            )
            .await
            .unwrap()
            .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !fixture
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|r| r["method"] == "thread/items/list")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            // The recovery is held on a separate reader; foreground metadata
            // still works and repeated selections cannot fan out more readers.
            driver.get_session("live").await.unwrap();
            driver
                .schedule_question_recovery("live", Some("paginated"))
                .await;
            assert_eq!(driver.question_recoveries.lock().await.scheduled.len(), 1);
            assert_eq!(
                fixture
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r["method"] == "thread/items/list")
                    .count(),
                1
            );
            if scenario == "cancelled" {
                fixture.client.stop();
            } else if scenario != "pending" {
                let (method, params) = match scenario {
                    "completed" => (
                        "turn/completed",
                        json!({"threadId":"live","turn":{"id":"turn","status":"completed"}}),
                    ),
                    "new_turn" => (
                        "turn/started",
                        json!({"threadId":"live","turn":{"id":"next","status":"inProgress"}}),
                    ),
                    _ => ("thread/closed", json!({"threadId":"live"})),
                };
                fixture
                    .send
                    .send(json!({"method":method,"params":params}))
                    .await
                    .unwrap();
                loop {
                    let event = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        fixture.events.recv(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    if matches!(event, CodexEvent::Notification {method: current, ..} if current == method)
                    {
                        break;
                    }
                }
            }
            gate.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !driver.question_recoveries.lock().await.scheduled.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let mut recovered = 0;
            while let Ok(event) = fixture.events.try_recv() {
                if matches!(event, CodexEvent::RecoveredQuestions { .. }) {
                    recovered += 1;
                }
            }
            assert_eq!(recovered, usize::from(scenario == "pending"), "{scenario}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn question_recovery_queues_fifth_session_without_reselection() {
        use super::super::CodexEvent;
        use crate::agents::adapters::codex::client::websocket::tests::Fixture as SharedFixture;
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut fixture = SharedFixture::with_deferred(|request| match request["method"].as_str().unwrap() {
            "thread/read" | "thread/resume" => json!({"thread":{"id":request["params"]["threadId"],"historyMode":"paginated","status":{"type":"active","activeFlags":[]}}}),
            "thread/turns/list" => json!({"data":[{"id":"turn","status":"inProgress"}]}),
            "thread/items/list" => json!({"data":[{"turnId":request["params"]["turnId"],"item":{"id":"q","type":"agentMessage","questions":[{"title":"Pending?"}]}}],"nextCursor":null}),
            other => panic!("unexpected RPC {other}"),
        }, Some(("thread/items/list", gate.clone()))).await;
        let driver = CodexDriver::new(fixture.client.clone());
        for n in 0..5 {
            driver
                .prepare_selected_session(&format!("thread-{n}"))
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "thread/items/list")
                .count()
                < 4
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        {
            let state = driver.question_recoveries.lock().await;
            assert_eq!(state.workers, 4);
            assert_eq!(state.pending.len(), 1);
            assert_eq!(state.scheduled.len(), 5);
        }
        fixture.send.send(json!({"method":"turn/started","params":{"threadId":"thread-4","turn":{"id":"next-turn","status":"inProgress"}}})).await.unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), fixture.events.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if matches!(event, CodexEvent::Notification {method, ..} if method == "turn/started") {
                break;
            }
        }
        driver
            .schedule_question_recovery("thread-4", Some("paginated"))
            .await;
        assert_eq!(driver.question_recoveries.lock().await.pending.len(), 1);
        gate.notify_waiters();
        // A permit is retained even if the fifth worker has not reached its
        // fixture gate yet. No new selection/refresh is needed to trigger it.
        gate.notify_one();
        let mut recovered = HashMap::new();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while recovered.len() < 5 {
                if let Some(CodexEvent::RecoveredQuestions { thread, turn, .. }) =
                    fixture.events.recv().await
                {
                    recovered.insert(thread, turn);
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            recovered.get("thread-4").map(String::as_str),
            Some("next-turn")
        );
        assert!(fixture.client.is_ready());
    }

    #[tokio::test]
    async fn legacy_turn_pages_open_large_threads_and_preserve_cursor_until_acceptance() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let invalid = Arc::new(AtomicBool::new(false));
        let invalid_response = invalid.clone();
        let mut fixture = Fixture::new(move |request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                assert_eq!(request["params"]["includeTurns"], false, "never hydrate all 30,000 historical items");
                let mut thread = golden()["thread"].clone();
                thread["historyMode"] = json!("legacy");
                json!({"thread":thread})
            }
            "thread/turns/list" => {
                assert_eq!(request["params"]["limit"], 1);
                assert_eq!(request["params"]["sortDirection"], "desc");
                assert_eq!(request["params"]["itemsView"], "full");
                let index = request["params"]["cursor"].as_str().map(|v| v.parse::<usize>().unwrap()).unwrap_or(300);
                assert!(index >= 298, "only the requested turns may be hydrated");
                let rows: Vec<_> = if index == 300 { vec![] } else {
                    (0..100).map(|n| json!({"id":format!("item-{index}-{n}"),"type":"agentMessage","text":format!("{index}-{n}")})).collect()
                };
                json!({"data":[{"id":format!("turn-{index:04}"),"itemsView":if invalid_response.load(Ordering::Acquire) {"notLoaded"} else {"full"},"items":rows}],"nextCursor":(index-1).to_string()})
            }
            other => panic!("unexpected RPC {other}"),
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        let tail = driver.get_timeline(&id, 80).await.unwrap();
        assert_eq!(tail.len(), 80);
        assert!(matches!(&tail[0].part, AgentPart::Text{text} if text == "299-20"));
        // Prepared/cancelled refreshes and malformed provider pages must leave
        // the accepted earlier cursor and residual untouched.
        drop(driver.prepare_timeline(&id, 80).await.unwrap());
        invalid.store(true, Ordering::Release);
        assert!(driver.get_timeline(&id, 80).await.is_err());
        invalid.store(false, Ordering::Release);
        let older = driver.load_earlier_timeline(&id, 80).await.unwrap();
        assert_eq!(older.len(), 80);
        assert!(matches!(&older[0].part, AgentPart::Text{text} if text == "298-40"));
        assert!(matches!(&older[79].part, AgentPart::Text{text} if text == "299-19"));
        assert!(older.last().unwrap().sort_key() < tail[0].sort_key());
        let ids: HashSet<_> = older.iter().chain(tail.iter()).map(|row| &row.id).collect();
        assert_eq!(ids.len(), 160);
    }

    // A notification after the fixture's RPC response proves the client reader
    // delivered that reply. The test can then poll precisely to the blocked
    // post-tail boundary, without sleeps or production instrumentation.
    async fn rpc_barrier(fixture: &mut Fixture, id: &str) {
        fixture
            .send(json!({"method":"synthetic/historyBarrier","params":{"threadId":id}}))
            .await;
        loop {
            let event = fixture.events.as_mut().unwrap().recv().await.unwrap();
            if matches!(event, super::super::CodexEvent::Notification {method,..} if method == "synthetic/historyBarrier")
            {
                break;
            }
        }
    }

    async fn refresh_interleaving(case: &str) {
        let mut fixture = Fixture::new(|r| match r["method"].as_str().unwrap() {
            "thread/read" => json!({"thread":golden()["thread"]}),
            "thread/items/list" => history_fixture_page(r, 241, false),
            _ => panic!("unexpected history RPC"),
        })
        .await;
        fixture.ready().await;
        let driver = Arc::new(CodexDriver::new(fixture.client.clone()));
        let memory = Arc::new(MemoryMirror::new());
        let service = SessionService::with_memory_mirror(driver.clone(), memory.clone());
        let id = AgentSessionId(golden()["thread"]["id"].as_str().unwrap().into());
        let tail = service.get_snapshot(&id).await.unwrap().timeline;
        let first = service.load_earlier_timeline(&id).await.unwrap();
        assert_eq!(first.len(), 80);
        while fixture.requests.try_recv().is_ok() {}

        let forms = driver.async_questions.lock().await;
        let mut refresh = Box::pin(service.refresh_snapshot(&id));
        loop {
            let request = tokio::select! {
                biased;
                result = &mut refresh => panic!("refresh must wait for forms: {result:?}"),
                request = fixture.next() => request,
            };
            if request["method"] == "thread/items/list" {
                break;
            }
        }
        rpc_barrier(&mut fixture, &id.0).await;
        std::future::poll_fn(|cx| {
            assert!(
                refresh.as_mut().poll(cx).is_pending(),
                "tail succeeded; forms are blocked"
            );
            Poll::Ready(())
        })
        .await;

        let mut earlier = None;
        if case == "earlier" {
            earlier = Some(service.load_earlier_timeline(&id).await.unwrap());
        } else if case == "conflict" {
            let mut info = memory.get_snapshot(&id).await.unwrap().info;
            info.title = "newer live metadata".into();
            memory.update_session(info).await;
        }
        if case == "cancel" {
            drop(refresh);
        } else {
            drop(forms);
            assert!(refresh.await.is_err(), "newer mirror state must win");
        }
        let remaining = match earlier {
            Some(rows) => rows,
            None => service.load_earlier_timeline(&id).await.unwrap(),
        };
        assert_eq!(
            remaining.len(),
            61,
            "failed refresh cannot reset the cursor at 180"
        );
        assert!(remaining.last().unwrap().sort_key() < first.first().unwrap().sort_key());
        let all: Vec<_> = tail.into_iter().chain(first).chain(remaining).collect();
        assert_eq!(all.len(), 241);
        assert_eq!(
            all.iter().map(|row| &row.id).collect::<HashSet<_>>().len(),
            241
        );
        assert!(service.load_earlier_timeline(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn codex_history_review_refresh_mirror_conflict_preserves_earlier_boundary() {
        refresh_interleaving("conflict").await;
    }

    #[tokio::test]
    async fn codex_history_review_cancel_before_mirror_acceptance_preserves_boundary() {
        refresh_interleaving("cancel").await;
    }

    #[tokio::test]
    async fn codex_history_review_refresh_and_earlier_never_overwrite_new_progress() {
        refresh_interleaving("earlier").await;
    }

    #[tokio::test]
    async fn codex_history_review_prepared_rollout_usage_wait_cancellation_does_not_reset_cursor() {
        let home = crate::test_support::asset_test_dir("history-prepared-rollout");
        std::fs::create_dir_all(home.join("sessions/day")).unwrap();
        let path = home.join("sessions/day/rollout.jsonl");
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            &path,
            format!("{}\n", json!({"type":"session_meta","payload":{"id":id}})),
        )
        .unwrap();
        let mut thread = golden()["thread"].clone();
        thread["id"] = json!(id);
        thread["path"] = json!(path);
        let source = thread.clone();
        let mut fixture = Fixture::new_in_home(
            move |r| match r["method"].as_str().unwrap() {
                "thread/read" => json!({"thread":source}),
                "thread/items/list" => history_fixture_page(r, 241, false),
                _ => panic!("unexpected history RPC"),
            },
            &home,
        )
        .await;
        fixture.ready().await;
        let mut driver = CodexDriver::new(fixture.client.clone());
        driver.observe_terminals(Some(crate::agents::terminal_runtime::TerminalRuntime::new(
            &[],
        )));
        assert!(
            super::super::rollout::Rollouts::prepare(&driver, &thread)
                .await
                .is_some(),
            "exercise the real prepared rollout branch"
        );
        driver.get_timeline(&id, 100).await.unwrap();
        driver.load_earlier_timeline(&id, 80).await.unwrap();
        while fixture.requests.try_recv().is_ok() {}
        let usage = driver.usage.lock().await;
        let mut read = Box::pin(driver.get_timeline(&id, 100));
        loop {
            let request = tokio::select! {
                biased;
                result = &mut read => panic!("usage lock must block the prepared rollout: {result:?}"),
                request = fixture.next() => request,
            };
            if request["method"] == "thread/items/list" {
                break;
            }
        }
        rpc_barrier(&mut fixture, &id).await;
        std::future::poll_fn(|cx| {
            assert!(
                read.as_mut().poll(cx).is_pending(),
                "RPC completed; usage remains blocked"
            );
            Poll::Ready(())
        })
        .await;
        drop(read);
        drop(usage);
        assert_eq!(
            driver.load_earlier_timeline(&id, 80).await.unwrap().len(),
            61
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn codex_history_review_stale_ack_cannot_change_initial_or_refreshed_mirror() {
        for case in ["revision", "incarnation", "initial"] {
            let mut fixture = Fixture::new(|r| match r["method"].as_str().unwrap() {
                "thread/read" => json!({"thread":golden()["thread"]}),
                "thread/items/list" => history_fixture_page(r, 241, false),
                _ => panic!("unexpected history RPC"),
            })
            .await;
            fixture.ready().await;
            let driver = Arc::new(CodexDriver::new(fixture.client.clone()));
            let memory = Arc::new(MemoryMirror::new());
            let service = SessionService::with_memory_mirror(driver.clone(), memory.clone());
            let id = AgentSessionId(golden()["thread"]["id"].as_str().unwrap().into());
            if case == "initial" {
                memory
                    .update_session(driver.get_session(&id.0).await.unwrap())
                    .await;
                driver.get_timeline(&id.0, 100).await.unwrap();
                driver.load_earlier_timeline(&id.0, 80).await.unwrap();
            } else {
                service.get_snapshot(&id).await.unwrap();
                service.load_earlier_timeline(&id).await.unwrap();
            }
            let held = memory.get_snapshot(&id).await.unwrap();
            let (seq, incarnation) = memory.snapshot_read_revision(&id).await;
            let gate = memory.history_gate(&id).await;
            let initial_revision = memory.history_read_revision(&id).await.unwrap();
            let prepared = driver.prepare_timeline(&id.0, 100).await.unwrap();
            if case == "incarnation" {
                memory.remove_session(&id).await;
                let mut recreated = held.info.clone();
                recreated.title = "recreated incarnation".into();
                for _ in 0..held.seq {
                    memory.update_session(recreated.clone()).await;
                }
                assert_eq!(
                    memory.get_snapshot(&id).await.unwrap().seq,
                    held.seq,
                    "ABA sequence matches"
                );
            } else {
                // Deliberately advance only the adapter, not the mirror seq.
                // A service-only lock or mirror-only fence cannot catch this.
                assert_eq!(
                    driver.load_earlier_timeline(&id.0, 80).await.unwrap().len(),
                    61
                );
            }
            if case == "initial" {
                assert!(memory
                    .initialize_prepared_history(
                        &id,
                        &gate,
                        initial_revision,
                        prepared.items,
                        prepared.commit
                    )
                    .await
                    .is_err());
                assert!(!memory.history_initialized(&id).await);
                assert!(memory.get_snapshot(&id).await.unwrap().timeline.is_empty());
            } else {
                let mut snapshot = held.clone();
                snapshot.timeline = prepared.items;
                let result = memory
                    .refresh_prepared_history(snapshot, (seq, incarnation), prepared.commit)
                    .await;
                if case == "incarnation" {
                    assert!(
                        result.unwrap().is_none(),
                        "old incarnation cannot ACK a new session"
                    );
                    let current = memory.get_snapshot(&id).await.unwrap();
                    assert_eq!(current.info.title, "recreated incarnation");
                    assert!(current.timeline.is_empty());
                    assert_eq!(
                        driver.load_earlier_timeline(&id.0, 80).await.unwrap().len(),
                        61
                    );
                } else {
                    assert!(matches!(result, Err(AgentError::HistoryCursorLost)));
                    let current = memory.get_snapshot(&id).await.unwrap();
                    assert_eq!(current.seq, held.seq);
                    assert_eq!(
                        current.timeline, held.timeline,
                        "adapter rejection must not alter mirror rows"
                    );
                }
            }
            assert!(driver
                .load_earlier_timeline(&id.0, 80)
                .await
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn codex_mcp_probe_skips_idle_history_and_preserves_app_cursor() {
        let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (state, count) = (active.clone(), reads.clone());
        let mut fixture = Fixture::new(move |r| match r["method"].as_str().unwrap() {
            "thread/read" => {
                let mut thread = golden()["thread"].clone();
                thread["status"] = json!({"type": if state.load(std::sync::atomic::Ordering::SeqCst) { "active" } else { "idle" }});
                json!({"thread":thread})
            }
            "thread/items/list" => {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                history_fixture_page(r, 241, false)
            }
            _ => panic!("unexpected MCP probe mutation"),
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        assert!(driver.mcp_call_timeline(&id, 20).await.unwrap().is_none());
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        active.store(true, std::sync::atomic::Ordering::SeqCst);
        driver.get_timeline(&id, 100).await.unwrap();
        driver.load_earlier_timeline(&id, 80).await.unwrap();
        assert!(driver.mcp_call_timeline(&id, 20).await.unwrap().is_some());
        assert_eq!(
            driver.load_earlier_timeline(&id, 80).await.unwrap().len(),
            61
        );
        assert_eq!(driver.history_preparations.available_permits(), 4);
    }

    #[tokio::test]
    async fn codex_history_review_prepared_leases_are_bounded_and_drop_does_not_consume() {
        let mut fixture = Fixture::new(|r| match r["method"].as_str().unwrap() {
            "thread/read" => json!({"thread":golden()["thread"]}),
            "thread/items/list" => history_fixture_page(r, 241, false),
            _ => panic!("unexpected history RPC"),
        })
        .await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        driver.get_timeline(&id, 100).await.unwrap();
        driver.load_earlier_timeline(&id, 80).await.unwrap();
        let mut candidates = Vec::new();
        for _ in 0..4 {
            candidates.push(driver.prepare_timeline(&id, 100).await.unwrap());
        }
        while fixture.requests.try_recv().is_ok() {}
        assert!(matches!(
            driver.prepare_timeline(&id, 100).await,
            Err(AgentError::NotAvailable(_))
        ));
        assert!(
            fixture.requests.try_recv().is_err(),
            "capacity rejection precedes provider I/O"
        );
        drop(candidates.pop());
        candidates.push(driver.prepare_timeline(&id, 100).await.unwrap());
        assert_eq!(
            driver.load_earlier_timeline(&id, 80).await.unwrap().len(),
            61,
            "uncommitted tails do not hold the read lock or reset pagination"
        );
        drop(candidates);
        assert_eq!(driver.history_preparations.available_permits(), 4);
        let held = driver.history_state.lock().unwrap().pages[&id].clone();
        driver.history_state.lock().unwrap().revision = u64::MAX;
        assert!(driver
            .accept_history(&id, Some(held.revision), held.clone())
            .is_err());
        assert_eq!(
            driver.history_state.lock().unwrap().pages[&id].revision,
            held.revision
        );
    }

    #[tokio::test]
    async fn codex_history_review_progress_during_metadata_preparation_fences_tail_ack() {
        let mut fixture = Fixture::new(|r| match r["method"].as_str().unwrap() {
            "thread/read" => json!({"thread":golden()["thread"]}),
            "thread/items/list" => history_fixture_page(r, 3, true),
            _ => panic!("unexpected history RPC"),
        })
        .await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        let tail = driver.get_timeline(&id, 100).await.unwrap();
        while fixture.requests.try_recv().is_ok() {}
        let writer = fixture.writer.clone();
        let response_block = writer.lock().await;
        let mut candidate = Box::pin(driver.prepare_timeline(&id, 100));
        let request = tokio::select! {
            biased;
            _ = &mut candidate => panic!("preparation must first request metadata"),
            request = fixture.next() => request,
        };
        assert_eq!(request["method"], "thread/read");
        // These rows are already in a residual page, so earlier can advance
        // without another RPC while the tail's metadata reply is outstanding.
        let first = driver.load_earlier_timeline(&id, 80).await.unwrap();
        assert_eq!(first.len(), 80);
        drop(response_block);
        let prepared = candidate.await.unwrap();
        assert!(
            prepared.commit.unwrap()().is_err(),
            "the ACK must fence progress from the start of preparation, not just the item-page read"
        );
        let mut all = tail;
        all.extend(first);
        all.extend(driver.load_earlier_timeline(&id, 2000).await.unwrap());
        assert_eq!(all.len(), 303);
        assert_eq!(
            all.iter().map(|row| &row.id).collect::<HashSet<_>>().len(),
            303
        );
    }

    #[tokio::test]
    async fn history_order_repair_is_staged_and_commits_with_live_content_and_cursor() {
        use crate::agents::adapters::memory_mirror::MemoryMirror;
        use crate::agents::ports::mirror::SessionMirrorPort;
        let mut fixture = Fixture::new(|request| {
            assert_eq!(request["method"], "thread/items/list");
            let params = &request["params"];
            let (ids,next) = if params["sortDirection"] == "asc" {
                assert_eq!(params["turnId"], "turn");
                (vec!["first", "second", "third"], Value::Null)
            } else if params["cursor"] == "older" {
                (vec!["first"], Value::Null)
            } else { (vec!["third", "second"],json!("older")) };
            json!({"data":ids.into_iter().map(|id| json!({"turnId":"turn","item":{"id":id,"type":"agentMessage","text":id}})).collect::<Vec<_>>(),"nextCursor":next})
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let mirror = MemoryMirror::for_agent("codex");
        let asid = AgentSessionId("thread".into());
        let mut live = Vec::new();
        for id in ["third", "first", "second"] {
            live.extend(
                driver
                    .ordered_items(
                        "thread",
                        "turn",
                        &json!({"id":id,"type":"agentMessage","text":id}),
                        0,
                    )
                    .unwrap(),
            );
        }
        mirror.upsert_timeline_items(&asid, live.clone()).await;
        let gate = mirror.history_gate(&asid).await;
        let before_read = mirror.history_read_revision(&asid).await.unwrap();
        let original = driver.order_candidate("thread");
        let (mut candidate, epoch) = original.clone();
        let mut page = HistoryPage::default();
        let items = driver
            .read_history_batch("thread", 2, &mut page, &mut Budget::new(), &mut candidate)
            .await
            .unwrap();
        assert_eq!(
            driver.order_candidate("thread"),
            original,
            "preparation cannot mutate live order"
        );
        assert!(
            driver.history_state.lock().unwrap().pages.is_empty(),
            "unaccepted cursor is private"
        );
        let mut newest = live[0].clone();
        newest.part = AgentPart::Text {
            text: "new content during history read".into(),
        };
        mirror
            .upsert_timeline_items(&asid, vec![newest.clone()])
            .await;
        let fresh_ids = items.iter().map(|item| item.id.clone()).collect();
        assert!(mirror
            .initialize_prepared_history(
                &asid,
                &gate,
                before_read,
                items,
                Some(Box::new(|| driver.accept_history_order(
                    "thread",
                    None,
                    page,
                    (candidate, epoch),
                    fresh_ids
                )))
            )
            .await
            .unwrap());
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            snapshot
                .timeline
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                live[1].id.as_str(),
                live[2].id.as_str(),
                live[0].id.as_str()
            ]
        );
        assert_eq!(
            snapshot.timeline[2].part, newest.part,
            "ordering cannot replace newer text"
        );
        assert_eq!(
            driver.history_state.lock().unwrap().pages["thread"]
                .cursor
                .as_deref(),
            Some("older")
        );
        let revision = mirror.history_read_revision(&asid).await.unwrap();
        let earlier = driver.prepare_earlier_timeline("thread", 2).await.unwrap();
        let accepted = mirror
            .accept_earlier_history(&asid, &gate, revision, earlier.items, earlier.commit)
            .await
            .unwrap();
        assert_eq!(accepted.len(), 1);
        assert!(accepted[0].id.contains("/first:"));
        assert!(accepted[0].seq > 0);
        assert!(driver.history_state.lock().unwrap().pages["thread"]
            .cursor
            .is_none());
    }

    #[tokio::test]
    async fn canonical_completion_replaces_provisional_child_rows_atomically() {
        use crate::agents::adapters::memory_mirror::MemoryMirror;
        use crate::agents::ports::mirror::SessionMirrorPort;
        let mut fixture = Fixture::new(|_| panic!("completion must not read history")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let mirror = MemoryMirror::for_agent("codex");
        let (events, mut receiver) = tokio::sync::broadcast::channel(16);
        mirror.publish_history_to(events);
        let asid = AgentSessionId("thread".into());
        let mut spawn = json!({
            "type":"collabAgentToolCall", "id":"spawn", "tool":"spawnAgent",
            "status":"inProgress", "receiverThreadIds":[], "agentsStates":{}
        });
        let provisional = driver.ordered_items("thread", "turn", &spawn, 0).unwrap();
        assert_eq!(provisional.len(), 1);
        mirror
            .upsert_timeline_items(&asid, provisional.clone())
            .await;
        let unrelated = driver
            .ordered_items(
                "thread",
                "turn",
                &json!({
                    "type":"agentMessage", "id":"text", "text":"keep"
                }),
                0,
            )
            .unwrap();
        mirror.upsert_timeline_items(&asid, unrelated.clone()).await;
        let before = mirror.get_snapshot(&asid).await.unwrap();
        spawn["status"] = json!("completed");
        spawn["receiverThreadIds"] = json!(["child-a", "child-b"]);
        let prepared = driver
            .prepare_completed_turn("thread", "turn", &[spawn], true)
            .unwrap();
        assert_eq!(prepared.items.len(), 2);
        let children: Vec<_> = prepared.items.iter().map(|row| row.id.clone()).collect();
        mirror.accept_live_history(&asid, prepared).await.unwrap();
        let accepted = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(accepted.timeline.len(), 3);
        assert!(!accepted
            .timeline
            .iter()
            .any(|row| row.id == provisional[0].id));
        assert!(children
            .iter()
            .all(|id| accepted.timeline.iter().any(|row| &row.id == id)));
        assert!(accepted
            .timeline
            .iter()
            .any(|row| row.id == unrelated[0].id));
        let removed = receiver.try_recv().unwrap();
        let upsert = receiver.try_recv().unwrap();
        assert!(
            matches!(&removed, AgentDomainEvent::TimelineRemoved { ids, .. } if ids == &vec![provisional[0].id.clone()])
        );
        assert!(
            matches!(&upsert, AgentDomainEvent::TimelineUpsert { items, .. } if items.iter().all(|row| row.id != provisional[0].id))
        );
        assert!(removed.seq() < upsert.seq());
        assert_eq!(
            mirror.get_events_after(&asid, before.seq).await,
            Some(vec![removed, upsert])
        );
    }

    #[tokio::test]
    async fn canonical_completion_commits_rows_references_and_replay_and_fences_old_reads() {
        use crate::agents::adapters::memory_mirror::MemoryMirror;
        use crate::agents::ports::mirror::SessionMirrorPort;
        let mut fixture = Fixture::new(|_| panic!("completion must not read history")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let mirror = MemoryMirror::for_agent("codex");
        let (events, mut receiver) = tokio::sync::broadcast::channel(16);
        mirror.publish_history_to(events);
        let asid = AgentSessionId("thread".into());
        let values: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(|id| json!({"id":id,"type":"agentMessage","text":id}))
            .collect();
        let mut live = Vec::new();
        for index in [2, 0, 1] {
            live.extend(
                driver
                    .ordered_items("thread", "turn", &values[index], 0)
                    .unwrap(),
            );
        }
        mirror.upsert_timeline_items(&asid, live.clone()).await;
        mirror
            .add_permission(PermissionRequest {
                id: "approval".into(),
                asid: asid.clone(),
                action: "execute".into(),
                resources: vec![],
                save: vec![],
                prompt: "Run".into(),
                tool: None,
                source_message_id: Some(live[0].message_id.clone()),
                source_tool_call_id: None,
                metadata: None,
                message: None,
                options: vec![],
            })
            .await;
        let before = mirror.get_snapshot(&asid).await.unwrap();
        let obsolete = driver
            .prepare_completed_turn(
                "thread",
                "turn",
                &[values[2].clone(), values[0].clone(), values[1].clone()],
                true,
            )
            .unwrap();
        let prepared = driver
            .prepare_completed_turn("thread", "turn", &values, true)
            .unwrap();
        mirror.accept_live_history(&asid, prepared).await.unwrap();
        let accepted = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            accepted
                .timeline
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>(),
            vec![live[1].id.clone(), live[2].id.clone(), live[0].id.clone()]
        );
        assert_eq!(
            accepted.permissions[0].source_message_id,
            Some(accepted.timeline[2].message_id.clone())
        );
        let timeline = receiver.try_recv().unwrap();
        let permission = receiver.try_recv().unwrap();
        assert!(
            matches!(&timeline, AgentDomainEvent::TimelineUpsert {items, ..} if items == &accepted.timeline)
        );
        assert!(
            matches!(&permission, AgentDomainEvent::PermissionPending { request, .. } if request == &accepted.permissions[0])
        );
        assert!(timeline.seq() > before.seq && permission.seq() > timeline.seq());
        assert_eq!(
            mirror.get_events_after(&asid, before.seq).await,
            Some(vec![timeline, permission])
        );
        assert!(matches!(
            mirror.accept_live_history(&asid, obsolete).await,
            Err(AgentError::HistoryCursorLost)
        ));
        assert_eq!(
            mirror.get_snapshot(&asid).await.unwrap().timeline,
            accepted.timeline
        );
        assert!(receiver.try_recv().is_err());
        let partial = driver
            .prepare_completed_turn(
                "thread",
                "turn",
                &[values[2].clone(), values[0].clone()],
                false,
            )
            .unwrap();
        mirror.accept_live_history(&asid, partial).await.unwrap();
        assert_eq!(
            mirror
                .get_snapshot(&asid)
                .await
                .unwrap()
                .timeline
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>(),
            accepted
                .timeline
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn canonical_completion_orders_items_missed_before_the_first_live_event() {
        let mut fixture = Fixture::new(|_| panic!("pure ordering test")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let values: Vec<_> = ["older-a", "older-b", "live-c"]
            .into_iter()
            .map(|id| json!({"id":id,"type":"agentMessage","text":id}))
            .collect();
        let live = driver
            .ordered_items("thread", "turn", &values[2], 0)
            .unwrap();
        let rows = driver
            .ordered_turn_items("thread", "turn", &values)
            .unwrap();
        assert_eq!(rows[2].message_id, live[0].message_id);
        assert!(rows
            .windows(2)
            .all(|pair| pair[0].sort_key() < pair[1].sort_key()));
        let ids = values
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        driver.set_history_order("thread", "turn", &ids).unwrap();
    }

    #[tokio::test]
    async fn codex_history_can_fill_items_missing_between_live_anchors() {
        let mut fixture = Fixture::new(|_| panic!("order tests never make RPCs")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let first = driver.item_position("thread", "turn", "first").unwrap();
        let last = driver.item_position("thread", "turn", "last").unwrap();
        driver
            .set_history_order(
                "thread",
                "turn",
                &["first".into(), "middle".into(), "last".into()],
            )
            .unwrap();
        let middle = driver.item_position("thread", "turn", "middle").unwrap();
        assert!(first < middle && middle < last);
        assert_eq!(
            driver.item_position("thread", "turn", "first").unwrap(),
            first
        );
        assert_eq!(
            driver.item_position("thread", "turn", "last").unwrap(),
            last
        );
        driver
            .set_history_order_at(
                "thread",
                "turn",
                &[
                    "older".into(),
                    "first".into(),
                    "middle".into(),
                    "last".into(),
                ],
                true,
            )
            .unwrap();
        assert!(driver.item_position("thread", "turn", "older").unwrap() < first);
        assert_eq!(
            driver.item_position("thread", "turn", "middle").unwrap(),
            middle
        );
    }

    #[tokio::test]
    async fn codex_history_order_preserves_live_anchor_and_rejects_range_or_capacity_exhaustion() {
        let mut fixture = Fixture::new(|_| panic!("order tests never make RPCs")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let live = driver.item_position("thread", "turn", "live").unwrap();
        driver
            .set_history_order_at("thread", "turn", &["old".into()], true)
            .unwrap();
        assert!(driver.item_position("thread", "turn", "old").unwrap() < live);
        assert_eq!(
            driver.item_position("thread", "turn", "live").unwrap(),
            live
        );
        let key = ("thread".into(), "turn".into(), "old".into());
        driver.order.lock().unwrap().insert(key, 0);
        let before = driver.order.lock().unwrap().clone();
        assert!(driver
            .set_history_order_at("thread", "turn", &["older".into()], true)
            .is_err());
        assert_eq!(*driver.order.lock().unwrap(), before);
        driver
            .order
            .lock()
            .unwrap()
            .insert(("max".into(), "turn".into(), "last".into()), u64::MAX);
        assert!(driver.item_position("max", "turn", "overflow").is_err());
        assert!(driver
            .set_history_order("max", "turn", &["overflow".into()])
            .is_err());
        {
            let mut order = driver.order.lock().unwrap();
            for n in order.len()..8192 {
                order.insert(("capacity".into(), "turn".into(), n.to_string()), n as u64);
            }
        }
        assert!(driver
            .set_history_order_at("thread", "turn", &["new".into()], true)
            .is_err());
        assert_eq!(driver.order.lock().unwrap().len(), 8192);
        assert!(
            fixture.client.is_ready(),
            "history cache exhaustion must not stop the primary"
        );
    }

    /// The retention budget is shared: a session is never refused a page
    /// because others hold theirs. The least recently read of the others
    /// are evicted until the page fits; only a page that cannot fit on its
    /// own is refused, and then nothing is consumed or evicted.
    #[test]
    fn codex_history_retention_evicts_the_least_recently_read_session_first() {
        let mut history = HashMap::new();
        for (id, bytes, read_at_ms) in [
            ("stale", MAX_HISTORY_RETAINED_BYTES / 2, 1),
            ("recent", MAX_HISTORY_RETAINED_BYTES / 2 - 64, 2),
        ] {
            history.insert(
                id.into(),
                HistoryPage {
                    retained_bytes: bytes,
                    read_at_ms,
                    ..Default::default()
                },
            );
        }
        // Committing a page recomputes its size from its contents: empty.
        CodexDriver::commit_history(&mut history, "new", HistoryPage::default()).unwrap();
        assert!(history.contains_key("new"));
        assert!(history.contains_key("stale") && history.contains_key("recent"));
        // One over the session cap evicts the least recently read, not the newest.
        for n in history.len()..MAX_SESSIONS {
            history.insert(
                n.to_string(),
                HistoryPage {
                    read_at_ms: 10,
                    ..Default::default()
                },
            );
        }
        CodexDriver::commit_history(&mut history, "another", HistoryPage::default()).unwrap();
        assert_eq!(history.len(), MAX_SESSIONS);
        assert!(
            !history.contains_key("stale"),
            "the least recently read went"
        );
        assert!(history.contains_key("recent") && history.contains_key("another"));
    }
}

#[cfg(test)]
mod model_page_tests {
    use super::super::tests::{golden, Fixture};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn command_catalog_capability_tracks_runnable_rows_and_missing_snapshot() {
        let mut fixture = Fixture::new(|_| panic!("local command rows must not call Codex")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let mut catalog = AgentCatalog::default();
        let mut snapshot = crate::command_catalog::Snapshot {
            agent: "codex-cli".into(),
            version: "synthetic".into(),
            commands: ["/new", "/export", "/compact"]
                .into_iter()
                .map(|name| crate::command_catalog::CatalogCommand {
                    name: name.into(),
                    description: "Synthetic command".into(),
                    args_hint: None,
                })
                .collect(),
        };
        driver.attach_commands(&mut catalog, Some(&snapshot));
        assert_eq!(catalog.commands.len(), 1);
        assert_eq!(catalog.commands[0].name, "/compact");
        assert!(driver.slash_commands_listed());

        // Missing/invalid local files already load as None. Keep the existing
        // empty-directory policy, including after a previous successful read.
        driver.attach_commands(&mut catalog, None);
        assert!(catalog.commands.is_empty());
        assert!(!driver.slash_commands_listed());
        snapshot.commands.pop();
        driver.attach_commands(&mut catalog, Some(&snapshot));
        assert!(catalog.commands.is_empty());
        assert!(!driver.slash_commands_listed());
        assert!(fixture.requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn compaction_requires_owned_idle_thread_and_never_resumes_or_retries() {
        use std::sync::atomic::AtomicUsize;
        let status = Arc::new(AtomicUsize::new(0));
        let live = status.clone();
        let compacted = Arc::new(AtomicUsize::new(0));
        let writes = compacted.clone();
        let mut fixture = Fixture::new(move |request| {
            let mut thread = golden()["thread"].clone();
            thread["status"] = json!({"type":match live.load(Ordering::SeqCst) {0 => "idle", 1 => "active", 2 => "notLoaded", _ => "unknown"}});
            match request["method"].as_str().unwrap() {
                "thread/start" | "thread/read" => json!({"thread":thread}),
                "thread/compact/start" => {
                    assert_eq!(request["params"], json!({"threadId":thread["id"]}));
                    writes.fetch_add(1, Ordering::SeqCst);
                    json!({})
                }
                other => panic!("compaction must not call {other}"),
            }
        }).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let id = golden()["thread"]["id"].as_str().unwrap().to_owned();
        assert!(matches!(
            driver.compact_session(&id, None).await,
            Err(AgentError::InvalidRequest(_))
        ));
        driver.create_session(None, None, None).await.unwrap();
        for delivery in ["queue", "steer", "auto"] {
            assert!(matches!(
                driver.compact_session(&id, Some(delivery)).await,
                Err(AgentError::Unsupported(_))
            ));
        }
        for state in [1, 2, 3] {
            status.store(state, Ordering::SeqCst);
            assert!(matches!(
                driver.compact_session(&id, None).await,
                Err(AgentError::InvalidRequest(_))
            ));
        }
        assert_eq!(compacted.load(Ordering::SeqCst), 0);
        status.store(0, Ordering::SeqCst);
        driver.compact_session(&id, None).await.unwrap();
        assert_eq!(compacted.load(Ordering::SeqCst), 1);
        fixture.send(json!({"method":"turn/started","params":{"threadId":id,"turn":{"id":"active-turn"}}})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.client.active_turn(&id).is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            driver.compact_session(&id, None).await,
            Err(AgentError::InvalidRequest(_))
        ));
        assert_eq!(compacted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn persisted_usage_refresh_replaces_external_counts_but_not_live_native_counts() {
        let mut fixture =
            Fixture::new(|_| panic!("usage seeding must not call the provider")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        let count = |n| json!({"last":{"totalTokens":n},"modelContextWindow":1000});
        driver.record_usage("thread", &count(20)).await.unwrap();
        driver.seed_usage("thread", Some(&count(10)), false).await;
        let mut info = mapper::session(&golden()["thread"]).unwrap();
        driver
            .cached_usage("thread")
            .await
            .unwrap()
            .apply(&mut info);
        assert_eq!(info.context_usage.as_ref().unwrap().total_tokens, 20);
        driver.seed_usage("thread", Some(&count(10)), true).await;
        driver
            .cached_usage("thread")
            .await
            .unwrap()
            .apply(&mut info);
        assert_eq!(info.context_usage.as_ref().unwrap().total_tokens, 10);
        driver.seed_usage("thread", None, true).await;
        driver
            .cached_usage("thread")
            .await
            .unwrap()
            .apply(&mut info);
        assert!(info.tokens.is_none() && info.context_usage.is_none());
    }

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
