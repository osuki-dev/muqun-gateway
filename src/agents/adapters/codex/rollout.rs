//! Bounded read-only observers for provider-selected persisted conversations.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value;

use super::rollout_projection::{canonical_items, Signals};
use super::rollout_tail::RolloutTail;
use super::CodexDriver;
use crate::agents::domain::{
    RuntimeSource, RuntimeTerminal, SessionRuntime, TimelineItem, TranscriptSource,
};
use crate::agents::ports::agent::AgentError;

const MAX_FOLLOWERS: usize = 8;

pub(crate) struct Prepared {
    tail: RolloutTail,
    binding: Option<RuntimeTerminal>,
    pub usage: Option<Value>,
}
impl Prepared {
    pub(crate) fn external(&self) -> bool {
        self.binding.is_some()
    }
}
struct Entry {
    tail: Arc<Mutex<RolloutTail>>,
    binding: RuntimeTerminal,
    generation: u64,
    healthy: Option<Instant>,
}
pub(crate) struct Rollouts {
    entries: Mutex<HashMap<String, Entry>>,
    generation: std::sync::atomic::AtomicU64,
    preparations: Arc<tokio::sync::Semaphore>,
}
impl Default for Rollouts {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            generation: Default::default(),
            preparations: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
}
pub(crate) struct Update {
    pub generation: u64,
    pub items: Vec<TimelineItem>,
    pub usage: Option<Value>,
}
fn unavailable() -> AgentError {
    AgentError::Protocol("Codex persisted observation requires an explicit refresh".into())
}
impl Rollouts {
    pub(crate) fn decorate(&self, id: &str, mut runtime: SessionRuntime) -> SessionRuntime {
        runtime.transcript = Some(if runtime.source == RuntimeSource::Native {
            TranscriptSource::Live
        } else if self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .is_some_and(|entry| {
                runtime.terminal.as_ref() == Some(&entry.binding)
                    && entry.healthy.is_some_and(|at| at.elapsed().as_secs() < 10)
            })
        {
            TranscriptSource::Persisted
        } else {
            TranscriptSource::Snapshot
        });
        runtime
    }
    pub(crate) fn install(&self, id: &str, prepared: Prepared) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(binding) = prepared.binding else {
            entries.remove(id);
            return;
        };
        if entries.len() >= MAX_FOLLOWERS && !entries.contains_key(id) {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.generation)
                .map(|(id, _)| id.clone())
            {
                entries.remove(&oldest);
            }
        }
        let generation = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        entries.insert(
            id.into(),
            Entry {
                tail: Arc::new(Mutex::new(prepared.tail)),
                binding,
                generation,
                healthy: Some(Instant::now()),
            },
        );
    }
    pub(crate) fn sessions(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, entry)| entry.healthy.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(crate) fn current(&self, id: &str, generation: u64) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .is_some_and(|e| e.generation == generation && e.healthy.is_some())
    }
    pub(crate) fn fence(&self, id: &str, generation: u64) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
        {
            if entry.generation == generation {
                entry.healthy = None;
            }
        }
    }
    pub(crate) async fn prepare(driver: &CodexDriver, thread: &Value) -> Option<Prepared> {
        let permit = driver
            .rollouts
            .preparations
            .clone()
            .try_acquire_owned()
            .ok()?;
        let id = thread["id"].as_str()?;
        if thread["historyMode"] != "paginated" {
            return None;
        }
        let home = driver.client().codex_home()?.to_owned();
        let path = PathBuf::from(thread["path"].as_str()?);
        let runtime = driver.runtime(id, true).await?;
        let binding = if runtime.source == RuntimeSource::Terminal {
            runtime.terminal
        } else {
            None
        };
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut tail = RolloutTail::open(&home, &path, &id).ok()?;
            let mut usage = None;
            // Only the bounded initial overlap is sampled. Historical items are
            // supplied by the ordinary snapshot; never replay them as live rows.
            for _ in 0..4 {
                let batch = tail.poll().ok()?;
                for row in batch.records {
                    if row["type"] == "event_msg" && row["payload"]["type"] == "token_count" {
                        if let Some(value) =
                            super::rollout_projection::token_usage(&row["payload"]["info"])
                        {
                            usage = Some(value);
                        }
                    }
                }
                if batch.caught_up {
                    break;
                }
            }
            Some(Prepared {
                tail,
                binding,
                usage,
            })
        })
        .await
        .ok()
        .flatten()
    }
    pub(crate) async fn poll(
        &self,
        driver: &CodexDriver,
        id: &str,
    ) -> Option<Result<Update, AgentError>> {
        let (tail, binding, generation) = {
            let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
            let entry = entries.get(id)?;
            entry.healthy?;
            (entry.tail.clone(), entry.binding.clone(), entry.generation)
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let before = driver.runtime(id, true).await.ok_or_else(unavailable)?;
            if before.source != RuntimeSource::Terminal
                || before.terminal.as_ref() != Some(&binding)
            {
                return Err(unavailable());
            }
            // Native metadata selects the current rollout, including revert's
            // same-UUID/new-file case. This is small metadata, never a rescan.
            let metadata = driver.read_thread(id).await?;
            let path = metadata["path"]
                .as_str()
                .ok_or_else(unavailable)?
                .to_owned();
            let reader = tail.clone();
            let batch = tokio::task::spawn_blocking(move || {
                let mut tail = reader.lock().unwrap_or_else(|p| p.into_inner());
                if tail.path() != std::path::Path::new(&path) {
                    return Err(unavailable());
                }
                tail.poll().map_err(|_| unavailable())
            })
            .await
            .map_err(|_| unavailable())??;
            let signals = Signals::parse(id, batch.records)?;
            let items = canonical_items(driver, id, &signals.items).await?;
            let after = driver.runtime(id, true).await.ok_or_else(unavailable)?;
            if after.source != RuntimeSource::Terminal || after.terminal.as_ref() != Some(&binding)
            {
                return Err(unavailable());
            }
            // The selected file can change while canonical pages are read.
            if (!signals.items.is_empty() || signals.usage.is_some())
                && driver.read_thread(id).await?["path"] != metadata["path"]
            {
                return Err(unavailable());
            }
            tokio::task::spawn_blocking(move || {
                tail.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .verify()
                    .map_err(|_| unavailable())
            })
            .await
            .map_err(|_| unavailable())??;
            if !self.current(id, generation) {
                return Err(unavailable());
            }
            Ok(Update {
                generation,
                items,
                usage: signals.usage,
            })
        })
        .await
        .unwrap_or_else(|_| Err(unavailable()));
        if result.is_err() {
            self.fence(id, generation);
        } else if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
        {
            if entry.generation == generation {
                entry.healthy = Some(Instant::now());
            }
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "explicit read-only probe of an existing authorized thread"]
    async fn codex_persisted_read_only_probe() {
        let home = std::env::var_os("MUQUN_CODEX_DAEMON_PROBE_HOME")
            .map(PathBuf::from)
            .expect("explicit probe home required");
        let id = std::env::var("MUQUN_CODEX_PROBE_THREAD").expect("explicit probe thread required");
        let (client, _events) = super::super::daemon::connect_at(&home)
            .await
            .unwrap()
            .expect("existing daemon");
        let driver = CodexDriver::new(client.clone());
        let metadata = driver.read_thread(&id).await.unwrap();
        assert_eq!(metadata["historyMode"], "paginated");
        let mut tail = RolloutTail::open(
            client.codex_home().unwrap(),
            std::path::Path::new(metadata["path"].as_str().unwrap()),
            &id,
        )
        .unwrap();
        let mut targets = Vec::new();
        let mut usage = false;
        let mut initial_records = 0;
        for _ in 0..4 {
            let batch = tail.poll().unwrap();
            initial_records += batch.records.len();
            let signals = Signals::parse(&id, batch.records).unwrap();
            targets.extend(signals.items);
            usage |= signals.usage.is_some();
            if batch.caught_up {
                break;
            }
        }
        let suffix = &targets[targets.len().saturating_sub(3)..];
        let items = canonical_items(&driver, &id, suffix).await.unwrap();
        println!("provider_not_loaded={} initial_records={} signalled_items={} canonical_rows={} observed_usage={}", metadata["status"]["type"] == "notLoaded", initial_records,targets.len(),items.len(),usage);
        let mut appended = 0;
        for _ in 0..3 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let batch = tail.poll().unwrap();
            appended += batch.records.len();
            let signals = Signals::parse(&id, batch.records).unwrap();
            let _ = canonical_items(&driver, &id, &signals.items).await.unwrap();
        }
        assert_eq!(
            driver.read_thread(&id).await.unwrap()["path"],
            metadata["path"]
        );
        println!(
            "incremental_records={} path_identity_stable=true controls_sent=0",
            appended
        );
        client.stop();
    }
}
