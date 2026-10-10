//! The owner's other gateways this one can hand work to. Each was paired
//! through its own pairing flow, as a phone is, so this side holds an
//! ordinary device token for it (`kind: gateway` over there). Rows live in
//! the shared store; whether a peer is reachable is derived from when it
//! last answered, never stored as a state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

use super::peer_client::{self, PeerClient, PeerError};
use crate::platform::config::FleetConfig;
use crate::platform::store::sqlite::Migration;
use crate::tasks::domain::now_ms;
use crate::tasks::ports::TaskFuture;

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "fleet",
    steps: &[
        "
CREATE TABLE peers(
  peer_id TEXT PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL,
  token TEXT NOT NULL, device_id TEXT NOT NULL, transport_key TEXT,
  added_ms INTEGER NOT NULL, last_seen_ms INTEGER,
  capabilities TEXT NOT NULL DEFAULT '[]', manifest TEXT);
CREATE UNIQUE INDEX peers_by_name ON peers(name COLLATE NOCASE);
",
        "
ALTER TABLE peers ADD COLUMN server_id TEXT;
UPDATE peers SET server_id = peer_id;
CREATE UNIQUE INDEX peers_by_server ON peers(server_id);
",
    ],
};

/// A peer is reachable while its last successful call is this recent.
pub(crate) const REACHABLE_FOR: Duration = Duration::from_secs(120);
/// How long a ping's answer (reachability, capabilities) is good for.
pub(crate) const CHECK_EVERY: Duration = Duration::from_secs(60);
const PING_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, PartialEq)]
pub(crate) struct Peer {
    /// This gateway's own id for the link, minted when it was made.
    pub peer_id: String,
    /// The id the peer gave for itself in its claim answer: display data,
    /// never a key. Two links asserting one id are a conflict, not a merge.
    pub server_id: String,
    pub name: String,
    pub url: String,
    pub token: String,
    pub device_id: String,
    pub transport_key: Option<String>,
    pub added_ms: u64,
    pub last_seen_ms: Option<u64>,
    pub capabilities: Vec<String>,
    pub manifest: Option<Value>,
}

/// Never the token or the transport key.
impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("peer_id", &self.peer_id)
            .field("server_id", &self.server_id)
            .field("name", &self.name)
            .field("url", &self.url)
            .field("last_seen_ms", &self.last_seen_ms)
            .finish_non_exhaustive()
    }
}

fn recent(seen: Option<u64>, now_ms: u64) -> bool {
    seen.is_some_and(|at| now_ms.saturating_sub(at) <= REACHABLE_FOR.as_millis() as u64)
}

impl Peer {
    pub(crate) fn reachable(&self, now_ms: u64) -> bool {
        recent(self.last_seen_ms, now_ms)
    }

    pub(crate) fn view(&self, now_ms: u64) -> PeerView {
        PeerView {
            peer_id: self.peer_id.clone(),
            server_id: self.server_id.clone(),
            name: self.name.clone(),
            url: self.url.clone(),
            reachable: self.reachable(now_ms),
            last_seen_ms: self.last_seen_ms,
        }
    }
}

/// A peer as the phone sees it: no token, no key, no ids but its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PeerView {
    pub peer_id: String,
    pub server_id: String,
    pub name: String,
    pub url: String,
    pub reachable: bool,
    pub last_seen_ms: Option<u64>,
}

pub(crate) trait PeerStore: Send + Sync {
    /// Insert or replace by `peer_id`. Another row's name or `server_id` is
    /// an error (the service refuses both first).
    fn put<'a>(&'a self, peer: &'a Peer) -> TaskFuture<'a, ()>;
    fn list<'a>(&'a self) -> TaskFuture<'a, Vec<Peer>>;
    fn remove<'a>(&'a self, peer_id: &'a str) -> TaskFuture<'a, bool>;
    /// It answered at `at_ms`; `capabilities`/`manifest` replace the cached ones when given.
    fn seen<'a>(
        &'a self,
        peer_id: &'a str,
        at_ms: u64,
        capabilities: Option<Vec<String>>,
        manifest: Option<Value>,
    ) -> TaskFuture<'a, ()>;
}

/// When each peer last answered, in memory: written by [`PeerService`],
/// read (from Task G3) by the task service to present mirrors without a
/// store round trip.
#[derive(Default)]
pub(crate) struct PeerPresence(RwLock<HashMap<String, Option<u64>>>);

impl PeerPresence {
    pub(crate) fn seen(&self, peer_id: &str, at_ms: Option<u64>) {
        if let Ok(mut map) = self.0.write() {
            map.insert(peer_id.to_owned(), at_ms);
        }
    }

    pub(crate) fn gone(&self, peer_id: &str) {
        if let Ok(mut map) = self.0.write() {
            map.remove(peer_id);
        }
    }

    /// `(reachable, last_seen_ms)`; an unknown peer (unlinked, forgotten) is unreachable, never seen.
    #[allow(dead_code)] // read by the task service from Task G3
    pub(crate) fn of(&self, peer_id: &str, now_ms: u64) -> (bool, Option<u64>) {
        let seen = self
            .0
            .read()
            .ok()
            .and_then(|map| map.get(peer_id).copied())
            .flatten();
        (recent(seen, now_ms), seen)
    }
}

pub(crate) struct LinkRequest {
    pub url: String,
    pub name: String,
    pub request_id: String,
    pub code: String,
}

#[derive(Debug)]
pub(crate) enum LinkError {
    /// No gateway answered at that URL (or it did not answer in time).
    Unreachable,
    /// It answered and said no; its error code (an expired or wrong code).
    Refused(String),
    /// The URL is this gateway's own.
    IsSelf,
    /// Another peer already holds that name.
    NameTaken,
    /// A peer asserting the same id is already linked.
    Conflict,
    Store(anyhow::Error),
}

#[derive(Debug)]
pub(crate) enum RenameError {
    Invalid,
    /// Another peer already holds that name.
    Taken,
    Store(anyhow::Error),
}

/// Ask `peer` to revoke the device this gateway holds over there, with that
/// device's own token. Best effort: the answer only goes to the log.
async fn revoke(peer: &Peer) {
    let Ok(client) = PeerClient::for_peer(peer) else {
        return;
    };
    let path = format!("/api/pairings/{}", peer.device_id);
    if let Err(error) = client.json(Method::DELETE, &path, None).await {
        tracing::info!(peer = %peer.peer_id, "the peer did not confirm the revoke: {error}");
    }
}

/// The peers registry and the accept switch (`AppState.peers`).
pub(crate) struct PeerService {
    store: Arc<dyn PeerStore>,
    accept: AtomicBool,
    config_path: Option<PathBuf>,
    names: RwLock<Vec<String>>,
    /// Shared with the task service (G3).
    pub(crate) presence: Arc<PeerPresence>,
}

impl PeerService {
    /// `config_path`: where `set_accepts_delegations` writes the switch;
    /// `None` keeps it in memory (tests).
    pub(crate) fn new(
        store: Arc<dyn PeerStore>,
        config: &FleetConfig,
        config_path: Option<PathBuf>,
        presence: Arc<PeerPresence>,
    ) -> Self {
        Self {
            store,
            accept: AtomicBool::new(config.accept_delegations),
            config_path,
            names: RwLock::default(),
            presence,
        }
    }

    /// Fill the name cache and the presence map from the store, once at start.
    pub(crate) async fn load_names(&self) {
        if let Ok(peers) = self.store.list().await {
            for peer in &peers {
                self.presence.seen(&peer.peer_id, peer.last_seen_ms);
            }
        }
        self.refresh_names().await;
    }

    async fn refresh_names(&self) {
        let Ok(peers) = self.store.list().await else {
            return;
        };
        if let Ok(mut names) = self.names.write() {
            *names = peers.into_iter().map(|peer| peer.name).collect();
        }
    }

    pub(crate) async fn list(&self) -> anyhow::Result<Vec<Peer>> {
        self.store.list().await
    }

    pub(crate) async fn get(&self, peer_id: &str) -> Option<Peer> {
        let peers = self.store.list().await.ok()?;
        peers.into_iter().find(|peer| peer.peer_id == peer_id)
    }

    /// A peer by its name (any case) or its id.
    #[cfg_attr(not(test), allow(dead_code))] // `tasks.delegate(machine)` from Task G3
    pub(crate) async fn resolve(&self, machine: &str) -> Option<Peer> {
        let peers = self.store.list().await.ok()?;
        peers
            .into_iter()
            .find(|peer| peer.name.eq_ignore_ascii_case(machine) || peer.peer_id == machine)
    }

    /// `peer`, pinged first when its last answer is older than [`CHECK_EVERY`].
    /// A peer that no longer knows this gateway's token is forgotten (§10.10)
    /// and answered as `None`.
    pub(crate) async fn checked(&self, peer: Peer) -> Option<Peer> {
        let now = now_ms();
        if peer
            .last_seen_ms
            .is_some_and(|seen| now.saturating_sub(seen) < CHECK_EVERY.as_millis() as u64)
        {
            return Some(peer);
        }
        let Ok(client) = PeerClient::for_peer(&peer) else {
            return Some(peer);
        };
        match tokio::time::timeout(PING_TIMEOUT, client.ping()).await {
            Ok(Ok(())) => {
                let capabilities = tokio::time::timeout(PING_TIMEOUT, client.capabilities())
                    .await
                    .ok()
                    .and_then(Result::ok);
                let now = now_ms();
                if let Err(error) = self
                    .store
                    .seen(&peer.peer_id, now, capabilities.clone(), None)
                    .await
                {
                    tracing::warn!(peer = %peer.peer_id, "failed to record a peer's answer: {error:#}");
                }
                self.presence.seen(&peer.peer_id, Some(now));
                Some(Peer {
                    last_seen_ms: Some(now),
                    capabilities: capabilities.unwrap_or(peer.capabilities),
                    ..peer
                })
            }
            Ok(Err(PeerError::Unauthorized)) => {
                tracing::info!(peer = %peer.peer_id, "a peer no longer knows this gateway; forgetting it");
                self.forget(&peer.peer_id).await;
                None
            }
            Ok(Err(_)) | Err(_) => Some(peer),
        }
    }

    /// Pair with the gateway at `request.url` through the pending pairing
    /// `request.request_id`/`request.code`, prove the new credentials, and
    /// keep them. `own_id` is this gateway's `server_id`. Once the claim
    /// succeeded, every failure takes the device it enrolled over there back
    /// out (best effort), so a failed link leaves nothing behind on either side.
    pub(crate) async fn link(&self, own_id: &str, request: LinkRequest) -> Result<Peer, LinkError> {
        let url = request.url.trim_end_matches('/').to_owned();
        let claimed = peer_client::claim(&url, &request.request_id, &request.code)
            .await
            .map_err(|error| match error {
                PeerError::Unreachable(_) => LinkError::Unreachable,
                PeerError::Unauthorized => LinkError::Refused("invalid_pairing_code".into()),
                PeerError::Refused { code, .. } => LinkError::Refused(code),
            })?;
        let now = now_ms();
        let peer = Peer {
            peer_id: format!("peer-{}", uuid::Uuid::new_v4()),
            server_id: claimed.server_id,
            name: request.name.trim().to_owned(),
            url,
            token: claimed.token,
            device_id: claimed.device_id,
            transport_key: claimed.transport_key,
            added_ms: now,
            last_seen_ms: Some(now),
            capabilities: Vec::new(),
            manifest: None,
        };
        match self.keep(own_id, peer.clone()).await {
            Ok(peer) => Ok(peer),
            Err(error) => {
                revoke(&peer).await;
                Err(error)
            }
        }
    }

    /// The rest of [`link`](Self::link) after the claim.
    async fn keep(&self, own_id: &str, mut peer: Peer) -> Result<Peer, LinkError> {
        if peer.server_id == own_id {
            return Err(LinkError::IsSelf);
        }
        let peers = self.store.list().await.map_err(LinkError::Store)?;
        if peers.iter().any(|other| other.server_id == peer.server_id) {
            // Already linked under that id: a second link is never merged into
            // it (the id is only what the claimant said); remove the first.
            return Err(LinkError::Conflict);
        }
        if peers
            .iter()
            .any(|other| other.name.eq_ignore_ascii_case(&peer.name))
        {
            return Err(LinkError::NameTaken);
        }
        // The first authenticated call proves the new credentials (and, sealed,
        // the transport key) before they are kept; the public capability list
        // fills the cache.
        let client = PeerClient::for_peer(&peer).map_err(LinkError::Store)?;
        client.ping().await.map_err(|error| match error {
            PeerError::Unreachable(_) => LinkError::Unreachable,
            PeerError::Unauthorized => LinkError::Refused("invalid_token".into()),
            PeerError::Refused { code, .. } => LinkError::Refused(code),
        })?;
        peer.last_seen_ms = Some(now_ms());
        peer.capabilities = client.capabilities().await.unwrap_or_default();
        self.store.put(&peer).await.map_err(LinkError::Store)?;
        self.presence.seen(&peer.peer_id, peer.last_seen_ms);
        self.refresh_names().await;
        Ok(peer)
    }

    /// Forget `peer_id` and ask it to revoke this gateway's token, best
    /// effort: the link is over either way. `false`: no such peer.
    pub(crate) async fn unlink(&self, peer_id: &str) -> anyhow::Result<bool> {
        let Some(peer) = self.get(peer_id).await else {
            return Ok(false);
        };
        revoke(&peer).await;
        let removed = self.store.remove(peer_id).await?;
        self.presence.gone(peer_id);
        self.refresh_names().await;
        Ok(removed)
    }

    /// Call `peer_id` `name` here (§10.16). `None`: no such peer.
    pub(crate) async fn rename(
        &self,
        peer_id: &str,
        name: &str,
    ) -> Result<Option<Peer>, RenameError> {
        let name = name.trim();
        if name.is_empty() || !crate::valid_device_name(name) {
            return Err(RenameError::Invalid);
        }
        let peers = self.store.list().await.map_err(RenameError::Store)?;
        if peers
            .iter()
            .any(|peer| peer.peer_id != peer_id && peer.name.eq_ignore_ascii_case(name))
        {
            return Err(RenameError::Taken);
        }
        let Some(mut peer) = peers.into_iter().find(|peer| peer.peer_id == peer_id) else {
            return Ok(None);
        };
        peer.name = name.to_owned();
        self.store.put(&peer).await.map_err(RenameError::Store)?;
        self.refresh_names().await;
        Ok(Some(peer))
    }

    /// Drop the row only (the peer answered 401: it no longer knows us).
    pub(crate) async fn forget(&self, peer_id: &str) {
        if let Err(error) = self.store.remove(peer_id).await {
            tracing::warn!(peer = %peer_id, "failed to forget a peer: {error:#}");
        }
        self.presence.gone(peer_id);
        self.refresh_names().await;
    }

    /// It answered just now.
    #[allow(dead_code)] // the peer socket from Task G2
    pub(crate) async fn mark_seen(&self, peer_id: &str) {
        let now = now_ms();
        if let Err(error) = self.store.seen(peer_id, now, None, None).await {
            tracing::warn!(peer = %peer_id, "failed to record a peer's answer: {error:#}");
        }
        self.presence.seen(peer_id, Some(now));
    }

    /// The peers' names, for the `mentions` a task's `tasks.read` will list (G5).
    #[allow(dead_code)] // read by `tasks.read` `mentions` from Task G5
    pub(crate) fn names(&self) -> Vec<String> {
        self.names
            .read()
            .map(|names| names.clone())
            .unwrap_or_default()
    }

    pub(crate) fn accepts_delegations(&self) -> bool {
        self.accept.load(Ordering::SeqCst)
    }

    /// Flip the switch and write it to `config.json`, changing only
    /// `fleet.accept_delegations` in what is on disk now (`edit_config`).
    pub(crate) fn set_accepts_delegations(&self, on: bool) -> anyhow::Result<()> {
        if let Some(path) = &self.config_path {
            crate::platform::store::edit_config(path, |config| {
                config.fleet.accept_delegations = on;
            })?;
        }
        self.accept.store(on, Ordering::SeqCst);
        Ok(())
    }
}
