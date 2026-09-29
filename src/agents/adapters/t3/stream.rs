//! The shell and thread subscriptions, parsed into typed events.
//!
//! T3 has no single firehose. `orchestration.subscribeShell` carries every
//! project and thread *summary* (creation, title, model, session status,
//! pending flags), and `orchestration.subscribeThread` carries one thread's
//! *events*: the user message, assistant and reasoning deltas (as
//! `thread.message-sent` with `streaming: true`), activities (tool cards,
//! approvals, user-input requests), session status, checkpoints and
//! reverts. The listener keeps the shell subscription open for the life of
//! the connection and one thread subscription per watched thread.
//!
//! Each subscription starts with a `snapshot` (or a replay after the last
//! sequence it saw, when reconnecting), then a `synchronized` marker, then
//! live items. Sequences are remembered so a reconnect resumes without a
//! gap; a replay the server cannot serve comes back as a fresh snapshot,
//! which the consumer treats as a resync.
//!
//! The manager (`agents/manager/t3.rs`) folds [`T3StreamEvent`]s into
//! `AgentDomainEvent`s with the functions in `mapper`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::client::T3Client;
use super::rpc::{RpcError, StreamItem, Subscription};

/// An item of the shell stream, typed.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    /// The full shell: `{projects, threads, snapshotSequence}`.
    Snapshot(Value),
    /// The initial snapshot or replay is done; what follows is live.
    Synchronized,
    ProjectUpserted {
        sequence: u64,
        project: Value,
    },
    ProjectRemoved {
        sequence: u64,
        project_id: String,
    },
    ThreadUpserted {
        sequence: u64,
        thread: Value,
    },
    ThreadRemoved {
        sequence: u64,
        thread_id: String,
    },
}

/// An item of one thread's stream, typed by event.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadEvent {
    /// `OrchestrationThreadDetailSnapshot`: `{snapshotSequence, thread, page?}`.
    Snapshot(Value),
    Synchronized,
    /// `thread.message-sent`. While `streaming` is true, `text` is a delta to
    /// append to the message; a final `streaming: false` with empty text
    /// closes it, with non-empty text replaces it.
    MessageSent {
        sequence: u64,
        message_id: String,
        role: String,
        text: String,
        streaming: bool,
        turn_id: Option<String>,
        /// When the message was created; every delta of one message carries
        /// the same value, so it is what orders the message's row.
        created_at: String,
        updated_at: String,
    },
    /// `thread.activity-appended`: the raw `OrchestrationThreadActivity`.
    ActivityAppended {
        sequence: u64,
        activity: Value,
    },
    /// `thread.session-set`: the raw `OrchestrationSession`.
    SessionSet {
        sequence: u64,
        session: Value,
    },
    /// `thread.turn-diff-completed`: a checkpoint for the turn.
    TurnDiffCompleted {
        sequence: u64,
        payload: Value,
    },
    /// `thread.reverted` to `turn_count`.
    Reverted {
        sequence: u64,
        turn_count: u64,
    },
    /// `thread.meta-updated`, `thread.runtime-mode-set`, `thread.interaction-mode-set`.
    MetaUpdated {
        sequence: u64,
        payload: Value,
    },
    Deleted {
        sequence: u64,
    },
    /// Any other `OrchestrationEvent`, by type.
    Other {
        sequence: u64,
        event_type: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum T3StreamEvent {
    Shell(ShellEvent),
    Thread {
        thread_id: String,
        event: ThreadEvent,
    },
    /// The shell subscription (and with it every thread subscription) is
    /// down; it will be resubscribed. Consumers should expect a snapshot
    /// or replay next.
    Disconnected {
        reason: String,
    },
    /// A thread subscription ended on its own (thread deleted, or the
    /// server closed it) and will not be resumed.
    ThreadClosed {
        thread_id: String,
        reason: String,
    },
}

/// Parse one shell stream item.
pub fn parse_shell_item(item: &Value) -> Option<ShellEvent> {
    let kind = item.get("kind").and_then(Value::as_str)?;
    let sequence = item.get("sequence").and_then(Value::as_u64).unwrap_or(0);
    Some(match kind {
        "snapshot" => ShellEvent::Snapshot(item.get("snapshot").cloned().unwrap_or(Value::Null)),
        "synchronized" => ShellEvent::Synchronized,
        "project-upserted" => ShellEvent::ProjectUpserted {
            sequence,
            project: item.get("project").cloned().unwrap_or(Value::Null),
        },
        "project-removed" => ShellEvent::ProjectRemoved {
            sequence,
            project_id: item.get("projectId").and_then(Value::as_str)?.to_string(),
        },
        "thread-upserted" => ShellEvent::ThreadUpserted {
            sequence,
            thread: item.get("thread").cloned().unwrap_or(Value::Null),
        },
        "thread-removed" => ShellEvent::ThreadRemoved {
            sequence,
            thread_id: item.get("threadId").and_then(Value::as_str)?.to_string(),
        },
        _ => return None,
    })
}

/// Parse one thread stream item.
pub fn parse_thread_item(item: &Value) -> Option<ThreadEvent> {
    let kind = item.get("kind").and_then(Value::as_str)?;
    match kind {
        "snapshot" => Some(ThreadEvent::Snapshot(
            item.get("snapshot").cloned().unwrap_or(Value::Null),
        )),
        "synchronized" => Some(ThreadEvent::Synchronized),
        "event" => parse_thread_event(item.get("event")?),
        _ => None,
    }
}

/// Parse one `OrchestrationEvent`.
pub fn parse_thread_event(event: &Value) -> Option<ThreadEvent> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    let sequence = event.get("sequence").and_then(Value::as_u64).unwrap_or(0);
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    let str_of = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
    Some(match event_type {
        "thread.message-sent" => ThreadEvent::MessageSent {
            sequence,
            message_id: str_of("messageId")?,
            role: str_of("role").unwrap_or_else(|| "assistant".into()),
            text: str_of("text").unwrap_or_default(),
            streaming: payload
                .get("streaming")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            turn_id: str_of("turnId"),
            created_at: str_of("createdAt")
                .or_else(|| str_of("updatedAt"))
                .unwrap_or_default(),
            updated_at: str_of("updatedAt")
                .or_else(|| str_of("createdAt"))
                .unwrap_or_default(),
        },
        "thread.activity-appended" => ThreadEvent::ActivityAppended {
            sequence,
            activity: payload.get("activity").cloned().unwrap_or(Value::Null),
        },
        "thread.session-set" => ThreadEvent::SessionSet {
            sequence,
            session: payload.get("session").cloned().unwrap_or(Value::Null),
        },
        "thread.turn-diff-completed" => ThreadEvent::TurnDiffCompleted { sequence, payload },
        "thread.reverted" => ThreadEvent::Reverted {
            sequence,
            turn_count: payload
                .get("turnCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        },
        "thread.meta-updated" | "thread.runtime-mode-set" | "thread.interaction-mode-set" => {
            ThreadEvent::MetaUpdated { sequence, payload }
        }
        "thread.deleted" => ThreadEvent::Deleted { sequence },
        other => ThreadEvent::Other {
            sequence,
            event_type: other.to_string(),
            payload,
        },
    })
}

/// The sequence an item carries, for resume bookkeeping.
fn sequence_of(item: &Value) -> Option<u64> {
    item.get("sequence")
        .and_then(Value::as_u64)
        .or_else(|| {
            item.get("event")
                .and_then(|e| e.get("sequence"))
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            item.get("snapshot")
                .and_then(|s| s.get("snapshotSequence"))
                .and_then(Value::as_u64)
        })
}

/// Most threads followed at once. Each is one open subscription on the
/// socket; the least recently watched goes first.
pub const MAX_WATCHED_THREADS: usize = 32;

enum Control {
    Watch(String),
    Unwatch(String),
}

/// A handle that starts and stops thread subscriptions, cheap to clone into
/// whatever learns that a session is being looked at.
#[derive(Clone)]
pub struct ThreadWatcher {
    control_tx: mpsc::Sender<Control>,
}

impl ThreadWatcher {
    /// Start following `thread_id`. Idempotent; a thread already followed
    /// becomes the most recently watched.
    pub fn watch(&self, thread_id: &str) {
        let _ = self
            .control_tx
            .try_send(Control::Watch(thread_id.to_string()));
    }

    pub fn unwatch(&self, thread_id: &str) {
        let _ = self
            .control_tx
            .try_send(Control::Unwatch(thread_id.to_string()));
    }

    /// A watcher whose commands go nowhere, for a driver without a listener.
    pub fn detached() -> Self {
        let (control_tx, _) = mpsc::channel(1);
        Self { control_tx }
    }
}

/// Runs the subscriptions and forwards typed events.
pub struct T3StreamListener {
    client: Arc<T3Client>,
    cancel: CancellationToken,
    control_tx: mpsc::Sender<Control>,
    control_rx: Option<mpsc::Receiver<Control>>,
}

impl T3StreamListener {
    pub fn new(client: Arc<T3Client>) -> Self {
        let (control_tx, control_rx) = mpsc::channel(64);
        Self {
            client,
            cancel: CancellationToken::new(),
            control_tx,
            control_rx: Some(control_rx),
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// A handle for starting and stopping thread subscriptions.
    pub fn watcher(&self) -> ThreadWatcher {
        ThreadWatcher {
            control_tx: self.control_tx.clone(),
        }
    }

    /// Start following `thread_id`. Idempotent.
    pub fn watch_thread(&self, thread_id: &str) {
        self.watcher().watch(thread_id);
    }

    pub fn unwatch_thread(&self, thread_id: &str) {
        self.watcher().unwatch(thread_id);
    }

    /// Spawn the listener. Events arrive on the returned receiver; the
    /// channel is bounded, so a consumer that stops reading pauses the
    /// socket rather than losing events.
    pub fn start(&mut self) -> mpsc::Receiver<T3StreamEvent> {
        let (tx, rx) = mpsc::channel(256);
        let Some(control_rx) = self.control_rx.take() else {
            return rx;
        };
        let task = ListenerTask {
            client: self.client.clone(),
            cancel: self.cancel.clone(),
            control_rx,
            events: tx,
            shell_seq: None,
            threads: HashMap::new(),
            order: VecDeque::new(),
        };
        tokio::spawn(task.run());
        rx
    }
}

struct WatchedThread {
    last_seq: Option<u64>,
    sub: Option<Subscription>,
}

struct ListenerTask {
    client: Arc<T3Client>,
    cancel: CancellationToken,
    control_rx: mpsc::Receiver<Control>,
    events: mpsc::Sender<T3StreamEvent>,
    shell_seq: Option<u64>,
    threads: HashMap<String, WatchedThread>,
    /// Watched thread ids, least recently watched first.
    order: VecDeque<String>,
}

impl ListenerTask {
    /// Add `id` to the watched set, or make it the most recent. Returns the
    /// thread that had to make room, if any; its subscription is dropped,
    /// which interrupts it on the server.
    fn remember(&mut self, id: &str) -> Option<String> {
        self.order.retain(|x| x != id);
        self.order.push_back(id.to_string());
        self.threads.entry(id.to_string()).or_insert(WatchedThread {
            last_seq: None,
            sub: None,
        });
        if self.threads.len() > MAX_WATCHED_THREADS {
            let oldest = self.order.pop_front()?;
            self.threads.remove(&oldest);
            return Some(oldest);
        }
        None
    }

    fn forget(&mut self, id: &str) {
        self.threads.remove(id);
        self.order.retain(|x| x != id);
    }

    async fn run(mut self) {
        let mut backoff = Duration::from_millis(500);
        loop {
            if self.cancel.is_cancelled() {
                break;
            }
            let shell = tokio::select! {
                _ = self.cancel.cancelled() => break,
                result = self.client.subscribe_shell(self.shell_seq) => result,
            };
            match shell {
                Ok(sub) => {
                    backoff = Duration::from_millis(500);
                    let reason = self.run_session(sub).await;
                    match reason {
                        None => break,
                        Some(reason) => {
                            tracing::warn!(%reason, "t3 shell stream ended; resubscribing");
                            self.drop_thread_subscriptions();
                            if self
                                .events
                                .send(T3StreamEvent::Disconnected { reason })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, wait_ms = backoff.as_millis() as u64, "t3 shell subscribe failed");
                }
            }
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
        tracing::info!("t3 stream listener stopped");
    }

    fn drop_thread_subscriptions(&mut self) {
        for watched in self.threads.values_mut() {
            watched.sub = None;
        }
    }

    /// Runs until the shell subscription ends. `None` means cancelled.
    async fn run_session(&mut self, mut shell: Subscription) -> Option<String> {
        // (Re)open every watched thread on this connection.
        let ids: Vec<String> = self.threads.keys().cloned().collect();
        for id in ids {
            self.open_thread(&id).await;
        }
        loop {
            // Thread subscriptions are polled together; `select_all` needs
            // at least one future, so an idle placeholder is added.
            let mut thread_polls = Vec::new();
            for (id, watched) in self.threads.iter_mut() {
                if let Some(sub) = watched.sub.as_mut() {
                    let id = id.clone();
                    thread_polls.push(Box::pin(async move { (id, sub.next().await) })
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<Output = (String, Option<StreamItem>)>
                                    + Send,
                            >,
                        >);
                }
            }
            let any_thread = async {
                if thread_polls.is_empty() {
                    std::future::pending::<(String, Option<StreamItem>)>().await
                } else {
                    futures::future::select_all(thread_polls).await.0
                }
            };
            tokio::select! {
                _ = self.cancel.cancelled() => return None,
                control = self.control_rx.recv() => match control {
                    Some(Control::Watch(id)) => {
                        if let Some(evicted) = self.remember(&id) {
                            tracing::debug!(thread_id = %evicted, "t3 thread subscription evicted");
                        }
                        if self.threads.get(&id).map(|w| w.sub.is_none()).unwrap_or(false) {
                            self.open_thread(&id).await;
                        }
                    }
                    Some(Control::Unwatch(id)) => self.forget(&id),
                    None => return None,
                },
                item = shell.next() => match item {
                    Some(StreamItem::Value(v)) => {
                        if let Some(seq) = sequence_of(&v) {
                            self.shell_seq = Some(seq);
                        }
                        if let Some(event) = parse_shell_item(&v) {
                            if self.events.send(T3StreamEvent::Shell(event)).await.is_err() {
                                return None;
                            }
                        }
                    }
                    Some(StreamItem::End(result)) => {
                        return Some(match result {
                            Ok(()) => "shell stream completed".into(),
                            Err(e) => e.to_string(),
                        });
                    }
                    None => return Some("shell stream dropped".into()),
                },
                (thread_id, item) = any_thread => {
                    match item {
                        Some(StreamItem::Value(v)) => {
                            if let Some(seq) = sequence_of(&v) {
                                if let Some(w) = self.threads.get_mut(&thread_id) {
                                    w.last_seq = Some(seq);
                                }
                            }
                            if let Some(event) = parse_thread_item(&v) {
                                let deleted = matches!(event, ThreadEvent::Deleted { .. });
                                if self.events.send(T3StreamEvent::Thread { thread_id: thread_id.clone(), event }).await.is_err() {
                                    return None;
                                }
                                if deleted {
                                    self.forget(&thread_id);
                                }
                            }
                        }
                        Some(StreamItem::End(result)) => {
                            let reason = match result {
                                Ok(()) => "completed".to_string(),
                                Err(RpcError::Disconnected) | Err(RpcError::Closed) => {
                                    // The shell stream ends too; it drives the reconnect.
                                    if let Some(w) = self.threads.get_mut(&thread_id) { w.sub = None; }
                                    continue;
                                }
                                Err(e) => e.to_string(),
                            };
                            self.forget(&thread_id);
                            if self.events.send(T3StreamEvent::ThreadClosed { thread_id, reason }).await.is_err() {
                                return None;
                            }
                        }
                        None => self.forget(&thread_id),
                    }
                }
            }
        }
    }

    async fn open_thread(&mut self, id: &str) {
        let last_seq = self.threads.get(id).and_then(|w| w.last_seq);
        match self.client.subscribe_thread(id, last_seq).await {
            Ok(sub) => {
                if let Some(w) = self.threads.get_mut(id) {
                    w.sub = Some(sub);
                }
            }
            Err(e) => {
                tracing::warn!(thread_id = %id, error = %e, "t3 thread subscribe failed");
                let _ = self
                    .events
                    .send(T3StreamEvent::ThreadClosed {
                        thread_id: id.to_string(),
                        reason: e.to_string(),
                    })
                    .await;
                self.forget(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The thread items captured during the approval probe, in order.
    fn captured_thread_items() -> Vec<Value> {
        let raw = include_str!("fixtures/thread_stream_items.json");
        serde_json::from_str(raw).expect("items")
    }

    fn captured_shell_items() -> Vec<Value> {
        let raw = include_str!("fixtures/shell_stream_items.json");
        serde_json::from_str(raw).expect("items")
    }

    #[test]
    fn parses_the_captured_shell_stream() {
        let items = captured_shell_items();
        let events: Vec<ShellEvent> = items.iter().filter_map(parse_shell_item).collect();
        assert_eq!(
            events.len(),
            items.len(),
            "every captured item is understood"
        );
        assert!(matches!(events[0], ShellEvent::Snapshot(_)));
        assert!(events.iter().any(|e| matches!(e, ShellEvent::Synchronized)));
        assert!(events
            .iter()
            .any(|e| matches!(e, ShellEvent::ThreadUpserted { .. })));
        let last_seq = items.iter().filter_map(sequence_of).max().unwrap();
        assert!(last_seq > 0);
    }

    #[test]
    fn parses_the_captured_thread_stream_in_order() {
        let items = captured_thread_items();
        let events: Vec<ThreadEvent> = items.iter().filter_map(parse_thread_item).collect();
        assert_eq!(events.len(), items.len());
        assert!(matches!(events[0], ThreadEvent::Snapshot(_)));
        assert!(matches!(events[1], ThreadEvent::Synchronized));
        // The user message, then streamed assistant deltas, then a close.
        let messages: Vec<&ThreadEvent> = events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::MessageSent { .. }))
            .collect();
        assert!(messages.len() >= 3);
        assert!(
            matches!(messages[0], ThreadEvent::MessageSent { role, streaming: false, .. } if role == "user")
        );
        let deltas: Vec<&ThreadEvent> = messages
            .iter()
            .copied()
            .filter(|e| matches!(e, ThreadEvent::MessageSent { role, streaming: true, .. } if role == "assistant"))
            .collect();
        assert!(
            !deltas.is_empty(),
            "assistant text arrives as streaming deltas"
        );
        let closes = messages.iter().filter(|e| {
            matches!(e, ThreadEvent::MessageSent { role, streaming: false, text, .. } if role == "assistant" && text.is_empty())
        });
        assert!(
            closes.count() >= 1,
            "an empty non-streaming message closes the stream"
        );
        // An approval round trip and a tool card.
        let kinds: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::ActivityAppended { activity, .. } => {
                    activity["kind"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&"approval.requested".to_string()));
        assert!(kinds.contains(&"approval.resolved".to_string()));
        assert!(kinds.contains(&"tool.completed".to_string()));
        assert!(events
            .iter()
            .any(|e| matches!(e, ThreadEvent::SessionSet { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, ThreadEvent::TurnDiffCompleted { .. })));
        // Sequences only ever grow.
        let seqs: Vec<u64> = items.iter().filter_map(sequence_of).collect();
        assert!(seqs.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn folds_deltas_the_way_the_projector_does() {
        // Mirrors apps/server/src/orchestration/projector.ts: streaming text
        // appends, a non-streaming non-empty text replaces, empty closes.
        let items = captured_thread_items();
        let mut messages: HashMap<String, (String, bool)> = HashMap::new();
        for e in items.iter().filter_map(parse_thread_item) {
            if let ThreadEvent::MessageSent {
                message_id,
                text,
                streaming,
                ..
            } = e
            {
                let entry = messages.entry(message_id).or_insert((String::new(), true));
                if streaming {
                    entry.0.push_str(&text);
                } else if !text.is_empty() {
                    entry.0 = text;
                }
                entry.1 = streaming;
            }
        }
        let assistant = messages
            .iter()
            .find(|(id, _)| id.starts_with("assistant:"))
            .map(|(_, v)| v)
            .expect("an assistant message");
        assert!(!assistant.0.is_empty());
        assert!(!assistant.1, "closed");
    }

    #[tokio::test]
    async fn the_watched_set_is_bounded_least_recent_first() {
        let client = Arc::new(T3Client::new(super::super::T3Endpoint::new(
            "http://127.0.0.1:9",
            super::super::T3Credential::None,
        )));
        let (tx, _rx) = mpsc::channel(1);
        let (_ctl, control_rx) = mpsc::channel(1);
        let mut task = ListenerTask {
            client: client.clone(),
            cancel: CancellationToken::new(),
            control_rx,
            events: tx,
            shell_seq: None,
            threads: HashMap::new(),
            order: VecDeque::new(),
        };
        for i in 0..MAX_WATCHED_THREADS {
            assert_eq!(task.remember(&format!("t{i}")), None);
        }
        // Watching t0 again makes it the most recent, so t1 goes first.
        assert_eq!(task.remember("t0"), None);
        assert_eq!(task.remember("new"), Some("t1".to_string()));
        assert_eq!(task.threads.len(), MAX_WATCHED_THREADS);
        task.forget("t0");
        assert!(!task.order.contains(&"t0".to_string()));
        assert_eq!(task.order.len(), task.threads.len());
        client.shutdown();
    }

    #[test]
    fn unknown_kinds_are_ignored_and_other_events_kept() {
        assert!(parse_shell_item(&serde_json::json!({"kind": "future-thing"})).is_none());
        let other = serde_json::json!({"kind": "event", "event": {"type": "thread.pinned", "sequence": 9, "payload": {"threadId": "t"}}});
        assert!(
            matches!(parse_thread_item(&other), Some(ThreadEvent::Other { event_type, sequence: 9, .. }) if event_type == "thread.pinned")
        );
        let deleted = serde_json::json!({"kind": "event", "event": {"type": "thread.deleted", "sequence": 3, "payload": {}}});
        assert!(matches!(
            parse_thread_item(&deleted),
            Some(ThreadEvent::Deleted { sequence: 3 })
        ));
    }
}
