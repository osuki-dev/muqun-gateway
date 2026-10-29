//! T3 V2 shell subscriptions and bounded thread projections.

use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Repository resolution refresh, not an application-log baseline.
    Enrichment {
        snapshot: Value,
        resolved_roots: Vec<String>,
    },
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
    V2Snapshot(Value),
    Deleted { sequence: u64 },
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
        .or_else(|| item.get("snapshotSequence").and_then(Value::as_u64))
}

pub fn parse_v2_shell_item(item: &Value) -> Option<ShellEvent> {
    let sequence = item.get("sequence").and_then(Value::as_u64);
    Some(match item.get("kind").and_then(Value::as_str)? {
        "snapshot" => {
            let snapshot = item.get("snapshot")?;
            snapshot.get("snapshotSequence")?.as_u64()?;
            snapshot.get("threads")?.as_array()?;
            snapshot.get("projects")?.as_array()?;
            if let Some(roots) = item.get("resolvedRepositoryIdentityRoots") {
                let resolved_roots = roots
                    .as_array()?
                    .iter()
                    .map(|root| root.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()?;
                return Some(ShellEvent::Enrichment {
                    snapshot: snapshot.clone(),
                    resolved_roots,
                });
            }
            ShellEvent::Snapshot(snapshot.clone())
        }
        "synchronized" => ShellEvent::Synchronized,
        "project.updated" => {
            item.pointer("/project/id")?.as_str()?;
            ShellEvent::ProjectUpserted {
                sequence: sequence?,
                project: item["project"].clone(),
            }
        }
        "project.removed" => ShellEvent::ProjectRemoved {
            sequence: sequence?,
            project_id: item["projectId"].as_str()?.to_string(),
        },
        "thread.updated" => {
            item.pointer("/thread/id")?.as_str()?;
            ShellEvent::ThreadUpserted {
                sequence: sequence?,
                thread: item["thread"].clone(),
            }
        }
        "thread.removed" => ShellEvent::ThreadRemoved {
            sequence: sequence?,
            thread_id: item["threadId"].as_str()?.to_string(),
        },
        _ => return None,
    })
}

fn duplicate(sequence: Option<u64>, last: Option<u64>, snapshot: bool) -> bool {
    sequence.zip(last).is_some_and(|(sequence, last)| {
        if snapshot {
            sequence < last
        } else {
            sequence <= last
        }
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
    rollback_runs: HashSet<String>,
    generation: Option<String>,
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
            rollback_runs: HashSet::new(),
            generation: None,
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
            {
                // Fresh HTTP control state per connection generation. Never
                // reuse a cursor from a previous environment/connection.
                self.shell_seq = None;
                for watched in self.threads.values_mut() {
                    watched.last_seq = None;
                }
                let snapshot = tokio::select! {_ = self.cancel.cancelled()=>break,result=self.client.shell_snapshot()=>result};
                match snapshot {
                    Ok(snapshot) => {
                        let seq = snapshot.get("snapshotSequence").and_then(Value::as_u64);
                        if seq.is_none()
                            || snapshot.get("threads").and_then(Value::as_array).is_none()
                        {
                            break;
                        }
                        if self
                            .events
                            .send(T3StreamEvent::Shell(ShellEvent::Snapshot(snapshot)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        self.shell_seq = seq;
                    }
                    Err(_) => {
                        tokio::select! {_ = self.cancel.cancelled()=>break,_=tokio::time::sleep(backoff)=>{}}
                        continue;
                    }
                }
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
                            // Decode/loss recovery is a resync, not a replay
                            // after an item we could not safely apply.
                            self.shell_seq = None;
                            for watched in self.threads.values_mut() {
                                watched.last_seq = None;
                            }
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
                    Some(StreamItem::Value(mut v)) => {
                        super::session_identity::expose(&mut v);
                        let seq=sequence_of(&v);
                        let event = {parse_v2_shell_item(&v)};
                        let enrichment=matches!(event,Some(ShellEvent::Enrichment{..}));
                        if !enrichment && duplicate(seq,self.shell_seq,v["kind"]=="snapshot") {continue;}
                        if let Some(event) = event {
                            if self.events.send(T3StreamEvent::Shell(event)).await.is_err() {
                                return None;
                            }
                            if !enrichment {if let Some(seq)=seq {self.shell_seq=Some(seq);}}
                        } else {
                            let kind=v["kind"].as_str().unwrap_or("");
                            let known={matches!(kind,"snapshot"|"synchronized"|"project.updated"|"project.removed"|"thread.updated"|"thread.removed")};
                            if !known && seq.is_some() {self.shell_seq=seq;continue;}
                            return Some("shell item decode failed".into());
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
                        Some(StreamItem::Value(mut v)) => {
                        super::session_identity::expose(&mut v);
                            let seq=sequence_of(&v);
                            let last=self.threads.get(&thread_id).and_then(|w|w.last_seq);
                            if duplicate(seq,last,v["kind"]=="snapshot") {continue;}
                            {
                                let cancel=self.cancel.clone();
                                let result=tokio::select!{_=cancel.cancelled()=>return None,result=self.apply_v2_item(&thread_id,&v)=>result};
                                match result {
                                    Ok(())=>continue,
                                    Err(error)=>return Some(error.to_string()),
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
        {
            let generation = match self.client.timeline_generation(id).await {
                Ok(generation) => generation,
                Err(error) => {
                    let _ = self
                        .events
                        .send(T3StreamEvent::Disconnected {
                            reason: error.to_string(),
                        })
                        .await;
                    return;
                }
            };
            if let Some(watched) = self.threads.get_mut(id) {
                if watched.generation.as_deref() != Some(&generation) {
                    watched.rollback_runs.clear();
                }
                watched.generation = Some(generation.clone());
            }
            let snapshot = tokio::select! {_=self.cancel.cancelled()=>return,result=self.client.thread_snapshot(id,None)=>result};
            match snapshot {
                Ok(snapshot) => {
                    let cancel = self.cancel.clone();
                    let snapshot = tokio::select! {_=cancel.cancelled()=>return,result=self.complete_rollback_snapshot(id,snapshot)=>match result {Ok(snapshot)=>snapshot,Err(_)=>return}};
                    let snapshot = match self
                        .client
                        .order_snapshot_in_generation(id, snapshot, &generation, true)
                        .await
                    {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            let _ = self
                                .events
                                .send(T3StreamEvent::Disconnected {
                                    reason: error.to_string(),
                                })
                                .await;
                            return;
                        }
                    };
                    let seq = snapshot["snapshotSequence"].as_u64();
                    if self
                        .events
                        .send(T3StreamEvent::Thread {
                            thread_id: id.to_string(),
                            event: ThreadEvent::V2Snapshot(snapshot),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if let Some(w) = self.threads.get_mut(id) {
                        w.last_seq = seq;
                    }
                }
                Err(error) => {
                    tracing::warn!(thread_id=%id,error=%error,"T3 V2 initial snapshot failed");
                    // A read failed, so ask the stream for a fresh bounded
                    // snapshot; never resume after an uninstalled cursor.
                    if let Some(w) = self.threads.get_mut(id) {
                        w.last_seq = None;
                    }
                }
            }
        }
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

    async fn apply_v2_item(
        &mut self,
        id: &str,
        item: &Value,
    ) -> Result<(), crate::agents::ports::agent::AgentError> {
        use crate::agents::ports::agent::AgentError;
        let generation = self
            .threads
            .get(id)
            .and_then(|watched| watched.generation.clone())
            .ok_or_else(|| AgentError::Protocol("V2 thread stream has no incarnation".into()))?;
        let snapshot = match item["kind"].as_str() {
            Some("synchronized") => return Ok(()),
            Some("snapshot") => {
                super::v2::projection::validate_snapshot(item, id)?;
                item.clone()
            }
            Some("event") => {
                let seq = item["sequence"]
                    .as_u64()
                    .ok_or_else(|| AgentError::Protocol("event without sequence".into()))?;
                if !super::v2::projection::validate_event(item, id)? {
                    if let Some(w) = self.threads.get_mut(id) {
                        w.last_seq = Some(seq);
                    }
                    return Ok(());
                }
                if item.pointer("/event/type").and_then(Value::as_str) == Some("thread.deleted") {
                    self.events
                        .send(T3StreamEvent::Thread {
                            thread_id: id.to_string(),
                            event: ThreadEvent::Deleted { sequence: seq },
                        })
                        .await
                        .map_err(|_| AgentError::Network("listener closed".into()))?;
                    self.forget(id);
                    return Ok(());
                }
                // Visible position is server-owned (forked/inherited rows need
                // not follow local ordinals). Refresh rather than inventing
                // positions or appending message.updated text as a delta.
                let snapshot = self.client.thread_snapshot(id, None).await?;
                if snapshot["snapshotSequence"]
                    .as_u64()
                    .is_none_or(|s| s < seq)
                {
                    return Err(AgentError::Protocol("snapshot behind live event".into()));
                }
                snapshot
            }
            _ => return Err(AgentError::Protocol("invalid V2 stream item".into())),
        };
        let snapshot = self.complete_rollback_snapshot(id, snapshot).await?;
        let snapshot = self
            .client
            .order_snapshot_in_generation(id, snapshot, &generation, true)
            .await?;
        let sequence = snapshot["snapshotSequence"].as_u64();
        self.events
            .send(T3StreamEvent::Thread {
                thread_id: id.to_string(),
                event: ThreadEvent::V2Snapshot(snapshot),
            })
            .await
            .map_err(|_| AgentError::Network("listener closed".into()))?;
        if let Some(w) = self.threads.get_mut(id) {
            w.last_seq = sequence;
        }
        Ok(())
    }

    async fn complete_rollback_snapshot(
        &mut self,
        id: &str,
        mut snapshot: Value,
    ) -> Result<Value, crate::agents::ports::agent::AgentError> {
        let rolled_back: HashSet<String> =
            super::v2::projection::rows(&snapshot["projection"], "runs")
                .iter()
                .filter(|run| run["status"] == "rolled_back")
                .filter_map(|run| run["id"].as_str().map(str::to_string))
                .collect();
        let new_rollback = self.threads.get(id).is_some_and(|watched| {
            rolled_back
                .iter()
                .any(|run| !watched.rollback_runs.contains(run))
        });
        if new_rollback && snapshot["hasMoreHistory"] == true {
            // A bounded window cannot prove deletion of retained older rows.
            // Ask for explicit refresh instead of downloading the whole
            // conversation and reconnecting forever at a cumulative budget.
            snapshot["gatewayRollbackRefresh"] = Value::Bool(true);
        }
        if let Some(watched) = self.threads.get_mut(id) {
            watched.rollback_runs = rolled_back;
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_complete_projection_cannot_advance_a_thread_cursor() {
        let client = Arc::new(T3Client::new(super::super::T3Endpoint::new(
            "http://127.0.0.1:9",
            super::super::T3Credential::None,
        )));
        let snapshot = |ids: &[&str], sequence: u64, complete: bool| {
            let mut value: Value =
                serde_json::from_str(include_str!("fixtures/v2/projection.json")).unwrap();
            value["snapshotSequence"] = serde_json::json!(sequence);
            value["hasMoreHistory"] = serde_json::json!(!complete);
            let sample = value["projection"]["visibleTurnItems"][0].clone();
            value["projection"]["visibleTurnItems"] = Value::Array(
                ids.iter()
                    .enumerate()
                    .map(|(position, id)| {
                        let mut row = sample.clone();
                        row["position"] = serde_json::json!(position);
                        row["sourceThreadId"] = serde_json::json!("thread");
                        row["sourceItemId"] = serde_json::json!(id);
                        row["item"]["id"] = serde_json::json!(id);
                        row
                    })
                    .collect(),
            );
            value
        };
        let generation = client.timeline_generation("thread").await.unwrap();
        for value in [
            snapshot(&["A", "B"], 10, false),
            snapshot(&["A"], 20, true),
            snapshot(&["A", "C"], 21, false),
        ] {
            client
                .order_snapshot_in_generation("thread", value, &generation, true)
                .await
                .unwrap();
        }
        let (_control_tx, control_rx) = mpsc::channel(1);
        let (events, mut received) = mpsc::channel(8);
        let mut task = ListenerTask {
            client: client.clone(),
            cancel: CancellationToken::new(),
            control_rx,
            events,
            shell_seq: Some(21),
            threads: HashMap::new(),
            order: VecDeque::new(),
        };
        task.remember("thread");
        let watched = task.threads.get_mut("thread").unwrap();
        watched.last_seq = Some(21);
        watched.generation = Some(generation);
        let mut stale = snapshot(&["A", "B"], 10, true);
        stale["kind"] = serde_json::json!("snapshot");
        assert!(task.apply_v2_item("thread", &stale).await.is_err());
        assert_eq!(task.threads["thread"].last_seq, Some(21));
        assert_eq!(task.shell_seq, Some(21));
        assert!(
            received.try_recv().is_err(),
            "no stale control projection may be forwarded"
        );
        client.shutdown();
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
}
