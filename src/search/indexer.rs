//! The background build and upkeep of the search index.
//!
//! One worker does every write, so two reads of a session never race:
//!
//! - a catch-up pass ([`CATCH_UP_EVERY`], and first [`STARTUP_DELAY`] after
//!   start) lists each source's sessions, compares each one's stamp with the
//!   watermark stored beside its rows, reads the changed ones newest first,
//!   and drops sessions a complete listing no longer has;
//! - between sessions and every [`FLUSH_EVERY`], the sessions the agent
//!   domain events touched are read again once quiet for [`QUIET`] (at most
//!   [`MAX_WAIT`] after the first event). A session of an agent without a
//!   source is indexed from the gateway's own mirror of it, which is what
//!   the App was shown.
//!
//! Each read is followed by a pause as long as it took (at least
//! [`MIN_PAUSE`]), so the build uses at most about half of one core, and
//! the writer runs on a thread of its own at a lower priority.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::index::{self, Mode, SessionMeta};
use super::sources::{self, Source};
use super::text;
use crate::agents::domain::{AgentDomainEvent, TimelineItem};
use crate::agents::ports::agent::AgentPort;
use crate::agents::ports::mirror::SessionMirrorPort as _;
use crate::tasks::transcript::{hide_in_items, Plumbing, Streaming};
use crate::AppState;

const STARTUP_DELAY: Duration = Duration::from_secs(20);
const CATCH_UP_EVERY: Duration = Duration::from_secs(300);
const FLUSH_EVERY: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_secs(10);
const MAX_WAIT: Duration = Duration::from_secs(120);
const MIN_PAUSE: Duration = Duration::from_millis(10);

pub(crate) fn spawn(state: AppState) {
    if state.search.writer().is_none() {
        return;
    }
    let mut events = state.agent_runtime.subscribe_events();
    let listener = state.clone();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match events.recv().await {
                Ok(event) => observe(&listener.search, &event),
                // Missed events are what the next catch-up pass is for.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
    tokio::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        let Some(mut worker) = Worker::new(state).await else {
            return;
        };
        let mut last_pass: Option<Instant> = None;
        loop {
            if last_pass.is_none_or(|at| at.elapsed() >= CATCH_UP_EVERY) {
                worker.catch_up().await;
                last_pass = Some(Instant::now());
            }
            worker.flush().await;
            tokio::select! {
                _ = tokio::time::sleep(FLUSH_EVERY) => {}
                _ = worker.state.search.wake.notified() => {}
            }
        }
    });
}

/// Note what an event says about its session for the worker.
pub(crate) fn observe(search: &super::SearchService, event: &AgentDomainEvent) {
    match event {
        AgentDomainEvent::SessionUpdated { asid, info, .. } => {
            let agent = (!info.agent_id.is_empty()).then(|| info.agent_id.clone());
            let meta = agent.as_deref().map(|agent| sources::meta(agent, info));
            search.touch(&asid.0, |dirty| {
                if agent.is_some() {
                    dirty.agent_id = agent;
                }
                if meta.is_some() {
                    dirty.meta = meta;
                }
                dirty.deleted = info.deleted;
            });
        }
        AgentDomainEvent::TimelineUpsert { asid, .. } => search.touch(&asid.0, |_| {}),
        AgentDomainEvent::TimelineRemoved { asid, ids, .. } => {
            search.touch(&asid.0, |dirty| dirty.removed.extend_from_slice(ids))
        }
        _ => {}
    }
}

struct Known {
    meta: SessionMeta,
    watermark: Option<String>,
}

struct Worker {
    state: AppState,
    writer: Arc<crate::platform::store::sqlite::SqliteStore>,
    known: HashMap<String, Known>,
}

impl Worker {
    async fn new(state: AppState) -> Option<Worker> {
        let writer = state.search.writer()?.clone();
        // The build yields to everything else on the machine.
        let _ = writer
            .call(|c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                // The first build writes a lot; the WAL need not stay that big.
                c.pragma_update(None, "journal_size_limit", 16 * 1024 * 1024)?;
                lower_thread_priority();
                Ok(())
            })
            .await;
        let stored = writer
            .call(|c| Ok(index::sessions(c)?))
            .await
            .map_err(|e| tracing::warn!("search index unreadable: {e}"))
            .ok()?;
        let known = stored
            .into_iter()
            .map(|(meta, watermark)| (meta.asid.clone(), Known { meta, watermark }))
            .collect();
        Some(Worker {
            state,
            writer,
            known,
        })
    }

    async fn catch_up(&mut self) {
        let started = Instant::now();
        let mut pending: Vec<(Source, Arc<dyn AgentPort>, SessionMeta, String)> = Vec::new();
        let mut listed_total = 0;
        let mut listed_agents: HashSet<String> = HashSet::new();
        for manager in self.state.agent_runtime.all_managers().await {
            let agent = manager.agent().clone();
            let agent_id = agent.kind();
            let Some(source) = Source::of(agent_id) else {
                continue;
            };
            let listing = match sources::list(source, &agent).await {
                Ok(listing) => listing,
                Err(error) => {
                    tracing::debug!("search: {agent_id} sessions not listed: {error}");
                    continue;
                }
            };
            listed_agents.insert(agent_id.to_owned());
            listed_total += listing.sessions.len();
            let listed: HashSet<&str> = listing
                .sessions
                .iter()
                .map(|(m, _)| m.asid.as_str())
                .collect();
            if listing.complete {
                let gone: Vec<String> = self
                    .known
                    .iter()
                    .filter(|(asid, k)| {
                        k.meta.agent_id == agent_id && !listed.contains(asid.as_str())
                    })
                    .map(|(asid, _)| asid.clone())
                    .collect();
                for asid in gone {
                    self.delete(agent_id, &asid).await;
                }
            }
            let before = pending.len();
            let count = listing.sessions.len();
            for (meta, stamp) in listing.sessions {
                let current = self.known.get(&meta.asid).is_some_and(|k| {
                    k.watermark.as_deref() == Some(stamp.as_str()) && k.meta == meta
                });
                if !current {
                    pending.push((source, agent.clone(), meta, stamp));
                }
            }
            tracing::debug!(
                "search: {agent_id} lists {count} sessions, {} to read",
                pending.len() - before
            );
        }
        let live_only = self
            .known
            .values()
            .filter(|k| !listed_agents.contains(&k.meta.agent_id))
            .count();
        let total = listed_total + live_only;
        let search = self.state.search.clone();
        let progress = &search.progress;
        progress.total.store(total, Ordering::Relaxed);
        progress
            .done
            .store(total - pending.len(), Ordering::Relaxed);
        pending.sort_by_key(|p| std::cmp::Reverse(p.2.updated_ms));
        let count = pending.len();
        for (source, agent, meta, stamp) in pending {
            self.index(source, &agent, meta, Some(&stamp)).await;
            progress.done.fetch_add(1, Ordering::Relaxed);
            self.flush().await;
        }
        if count > 0 {
            // A transcript read is a burst of allocation; give it back.
            crate::platform::server::trim_allocator();
            tracing::info!(
                "search: indexed {count} sessions in {:.1}s ({total} known)",
                started.elapsed().as_secs_f64()
            );
        }
    }

    /// Read `meta` until its watermark reaches `stamp` (once without one)
    /// or stops moving.
    async fn index(
        &mut self,
        source: Source,
        agent: &Arc<dyn AgentPort>,
        meta: SessionMeta,
        stamp: Option<&str>,
    ) {
        loop {
            let stored = self.known.get(&meta.asid).and_then(|k| k.watermark.clone());
            let started = Instant::now();
            let read = match sources::read(source, agent, &meta, stored.clone()).await {
                Ok(read) => read,
                Err(error) => {
                    tracing::debug!("search: {} {} not read: {error}", meta.agent_id, meta.asid);
                    // A whole-history read that failed (a transcript over the
                    // reader's limit) is not tried again until it changes.
                    if let Some(stamp) =
                        stamp.filter(|_| matches!(source, Source::Claude | Source::Pi))
                    {
                        self.write(&meta, Vec::new(), Mode::Merge, Some(stamp.to_owned()))
                            .await;
                    }
                    return;
                }
            };
            let watermark = read.watermark.clone();
            self.write(&meta, read.items, read.mode, Some(watermark.clone()))
                .await;
            pace(started.elapsed()).await;
            if stamp.is_none_or(|stamp| stamp == watermark)
                || stored.as_deref() == Some(watermark.as_str())
            {
                return;
            }
        }
    }

    async fn write(
        &mut self,
        meta: &SessionMeta,
        mut items: Vec<TimelineItem>,
        mode: Mode,
        watermark: Option<String>,
    ) {
        let task = self.state.tasks.is_task_session(&meta.asid).await;
        let rows = searchable(&mut items, task, meta.updated_ms);
        let (owned, mark) = (meta.clone(), watermark.clone());
        let now = crate::now_unix_ms() as u64;
        match self
            .writer
            .call(move |c| Ok(index::apply(c, &owned, &rows, mode, mark.as_deref(), now)?))
            .await
        {
            Ok(_) => {
                self.known.insert(
                    meta.asid.clone(),
                    Known {
                        meta: meta.clone(),
                        watermark,
                    },
                );
            }
            Err(error) => tracing::warn!("search: {} not written: {error}", meta.asid),
        }
    }

    async fn delete(&mut self, agent_id: &str, asid: &str) {
        let (agent, id) = (agent_id.to_owned(), asid.to_owned());
        if self
            .writer
            .call(move |c| Ok(index::delete_session(c, &agent, &id)?))
            .await
            .is_ok()
        {
            self.known.remove(asid);
        }
    }

    /// Read again the sessions events touched, once quiet.
    async fn flush(&mut self) {
        let ready: Vec<(String, super::Dirty)> = {
            let mut dirty = self
                .state
                .search
                .dirty
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let keys: Vec<String> = dirty
                .iter()
                .filter(|(_, d)| d.last.elapsed() >= QUIET || d.first.elapsed() >= MAX_WAIT)
                .map(|(asid, _)| asid.clone())
                .collect();
            keys.into_iter()
                .filter_map(|asid| dirty.remove(&asid).map(|d| (asid, d)))
                .collect()
        };
        for (asid, dirty) in ready {
            let started = Instant::now();
            self.refresh(&asid, dirty).await;
            pace(started.elapsed()).await;
        }
    }

    async fn refresh(&mut self, asid: &str, dirty: super::Dirty) {
        let known = self.known.get(asid).map(|k| k.meta.clone());
        let agent_id = match dirty
            .agent_id
            .clone()
            .or_else(|| known.as_ref().map(|m| m.agent_id.clone()))
        {
            Some(agent) => agent,
            None => match self.owner(asid).await {
                Some(agent) => agent,
                None => return,
            },
        };
        if dirty.deleted {
            self.delete(&agent_id, asid).await;
            return;
        }
        let Some(manager) = self.state.agent_runtime.manager_for_agent(&agent_id).await else {
            return;
        };
        let agent = manager.agent().clone();
        if let Some(source) = Source::of(&agent_id) {
            // The catch-up pass finds a session the index has never listed.
            let Some(mut meta) = dirty.meta.clone().or(known) else {
                return;
            };
            if meta.directory.is_none() {
                meta.directory = self.known.get(asid).and_then(|k| k.meta.directory.clone());
            }
            let stamp = match source {
                Source::Claude | Source::Codex => match sources::stamp(source, &meta).await {
                    Some(stamp) => Some(stamp),
                    None => return,
                },
                _ => None,
            };
            let held = self.known.get(asid).and_then(|k| k.watermark.clone());
            if stamp.is_some() && held == stamp {
                return;
            }
            self.index(source, &agent, meta, stamp.as_deref()).await;
            return;
        }
        let snapshot = manager.mirror().get_snapshot(&asid.into()).await;
        let meta = snapshot
            .as_ref()
            .map(|s| sources::meta(&agent_id, &s.info))
            .or(dirty.meta)
            .or(known);
        let Some(meta) = meta else {
            return;
        };
        if !dirty.removed.is_empty() {
            let (agent, id, ids) = (agent_id.clone(), asid.to_owned(), dirty.removed.clone());
            let _ = self
                .writer
                .call(move |c| Ok(index::remove_items(c, &agent, &id, &ids)?))
                .await;
        }
        let items = snapshot.map(|s| s.timeline).unwrap_or_default();
        self.write(&meta, items, Mode::Merge, Some("live".into()))
            .await;
    }

    /// The agent whose mirror holds `asid`, without asking any agent.
    async fn owner(&self, asid: &str) -> Option<String> {
        for manager in self.state.agent_runtime.all_managers().await {
            if manager.mirror().get_snapshot(&asid.into()).await.is_some() {
                return Some(manager.agent().kind().to_owned());
            }
        }
        None
    }
}

/// The rows of `items` as a reader is served them: a task session without
/// its completion instruction and consumed markers, and never a
/// skill-loading row (`tasks::transcript`).
pub(crate) fn searchable(
    items: &mut Vec<TimelineItem>,
    task: bool,
    fallback_ms: u64,
) -> Vec<text::Row> {
    hide_in_items(
        items,
        Plumbing {
            streaming: Streaming(false),
            task,
        },
    );
    text::rows(items, fallback_ms)
}

async fn pace(spent: Duration) {
    tokio::time::sleep(spent.max(MIN_PAUSE)).await;
}

/// Lower the calling thread's scheduling priority (Linux: per thread).
fn lower_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}
