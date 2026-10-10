//! The owner's other gateways this one can hand work to. Each was paired
//! through its own pairing flow, as a phone is, so this side holds an
//! ordinary device token for it (`kind: gateway` over there). Rows live in
//! the shared store; whether a peer is reachable is derived from when it
//! last answered, never stored as a state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::Value;

use super::peer_client::{self, PeerClient, PeerError};
use crate::connectivity::authority::{DeviceKind, DeviceRecord};
use crate::platform::config::FleetConfig;
use crate::platform::store::sqlite::Migration;
use crate::tasks::domain::now_ms;
use crate::tasks::ports::TaskFuture;
use crate::AppState;

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
/// A first manifest runs the peer's toolchain probes, which take a few seconds.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Whether two computer names are the same name: any case, Unicode-aware.
pub(crate) fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// `self` always means this computer, so no peer may be called that.
pub(crate) fn reserved_name(name: &str) -> bool {
    same_name(name, "self")
}

fn within(at: Option<u64>, now_ms: u64, span: Duration) -> bool {
    at.is_some_and(|at| now_ms.saturating_sub(at) < span.as_millis() as u64)
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
/// device's own token. Best effort: whether the peer confirmed it.
async fn revoke(peer: &Peer) -> bool {
    let Ok(client) = PeerClient::for_peer(peer) else {
        return false;
    };
    let path = format!("/api/pairings/{}", peer.device_id);
    match client.json(Method::DELETE, &path, None).await {
        Ok(_) => true,
        Err(error) => {
            tracing::info!(peer = %peer.peer_id, "the peer did not confirm the revoke: {error}");
            false
        }
    }
}

/// How often [`PeerService::mark_seen`] writes to the store.
const SEEN_WRITE_EVERY: Duration = Duration::from_secs(10);

/// The one task socket per peer (`fleet::mirror::watch`): started when a
/// mirror of that peer opens, ended by its follower when none is open.
#[derive(Default)]
pub(crate) struct PeerSockets {
    running: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl PeerSockets {
    /// Start `follow` for `peer_id` unless one is running.
    pub(crate) fn ensure(
        &self,
        peer_id: &str,
        follow: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) {
        let Ok(mut running) = self.running.lock() else {
            return;
        };
        if running
            .get(peer_id)
            .is_some_and(|handle| !handle.is_finished())
        {
            return;
        }
        running.insert(peer_id.to_owned(), follow());
    }

    pub(crate) fn is_live(&self, peer_id: &str) -> bool {
        self.running
            .lock()
            .ok()
            .and_then(|running| running.get(peer_id).map(|handle| !handle.is_finished()))
            .unwrap_or(false)
    }

    /// The calling follower of `peer_id` is about to end: its entry goes
    /// now, so a mirror bound from here on starts a new one (`ensure`).
    pub(crate) fn leave(&self, peer_id: &str) {
        let Ok(mut running) = self.running.lock() else {
            return;
        };
        let me = tokio::task::try_id();
        if running
            .get(peer_id)
            .is_some_and(|handle| Some(handle.id()) == me)
        {
            running.remove(peer_id);
        }
    }

    /// End `peer_id`'s follower, if one runs. The follower may be the
    /// caller (it forgets a peer that no longer knows this gateway), so
    /// callers do this last: nothing after it in the same task would run.
    pub(crate) fn stop(&self, peer_id: &str) {
        let handle = self
            .running
            .lock()
            .ok()
            .and_then(|mut running| running.remove(peer_id));
        if let Some(handle) = handle {
            handle.abort();
        }
    }
}

/// Revoke the gateway devices here that `server_id` paired as: the peer's
/// own login into this computer. Best effort, like the revoke over there.
/// `(found, revoked)`: whether a login was there, and whether it is gone.
/// The list changes only once it is written, so memory and disk never
/// disagree.
fn revoke_login(state: &AppState, server_id: &str) -> (bool, bool) {
    if server_id.is_empty() {
        return (false, false);
    }
    let Ok(mut devices) = state.devices.lock() else {
        return (false, false);
    };
    let kept: Vec<_> = devices
        .iter()
        .filter(|device| !is_login_of(device, server_id))
        .cloned()
        .collect();
    if kept.len() == devices.len() {
        return (false, false);
    }
    match crate::platform::store::write_devices(&kept) {
        Ok(()) => {
            *devices = kept;
            (true, true)
        }
        Err(error) => {
            tracing::warn!("failed to revoke a peer's login here: {error:#}");
            (true, false)
        }
    }
}

/// A gateway device (never an app's) that paired as exactly `server_id`.
fn is_login_of(device: &DeviceRecord, server_id: &str) -> bool {
    device.kind == DeviceKind::Gateway && device.server_id.as_deref() == Some(server_id)
}

/// What an unlink did besides forgetting the peer: each step runs whatever
/// the other did, and says how it went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Unlinked {
    /// The peer confirmed it revoked this gateway's token over there.
    pub remote_revoked: bool,
    /// A login of the peer's was here.
    pub counterpart_found: bool,
    /// ... and is revoked (false with `counterpart_found`: it could not be
    /// written).
    pub counterpart_revoked: bool,
    /// The peer's row is gone here.
    #[serde(skip)]
    pub forgotten: bool,
}

/// The peers registry and the accept switch (`AppState.peers`).
pub(crate) struct PeerService {
    store: Arc<dyn PeerStore>,
    accept: AtomicBool,
    config_path: Option<PathBuf>,
    names: RwLock<Vec<String>>,
    /// When each peer's manifest was last asked for, answered or not; in
    /// memory, so the first look after a restart asks again.
    manifests_at: Mutex<HashMap<String, u64>>,
    /// When each peer was last pinged, answered or not.
    pinged_at: Mutex<HashMap<String, u64>>,
    /// Peers a background refresh is running for: one at a time per peer.
    refreshing: Mutex<HashSet<String>>,
    /// Shared with the task service (G3).
    pub(crate) presence: Arc<PeerPresence>,
    pub(crate) sockets: PeerSockets,
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
            manifests_at: Mutex::default(),
            pinged_at: Mutex::default(),
            refreshing: Mutex::default(),
            presence,
            sockets: PeerSockets::default(),
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

    /// A peer by its name (any case), this gateway's id for it, or the id it
    /// gave for itself (`machineId` in `fleet.list`).
    pub(crate) async fn resolve(&self, machine: &str) -> Option<Peer> {
        let peers = self.store.list().await.ok()?;
        peers.into_iter().find(|peer| {
            same_name(&peer.name, machine) || peer.peer_id == machine || peer.server_id == machine
        })
    }

    /// Whether `peer` answers, as far as is known without asking it now:
    /// `None` when it was neither heard from nor pinged in the last [`CHECK_EVERY`].
    pub(crate) fn reachability(&self, peer: &Peer, now: u64) -> Option<bool> {
        if within(peer.last_seen_ms, now, CHECK_EVERY) {
            return Some(true);
        }
        let pinged = self
            .pinged_at
            .lock()
            .ok()
            .and_then(|at| at.get(&peer.peer_id).copied());
        within(pinged, now, CHECK_EVERY).then(|| peer.reachable(now))
    }

    /// Whether `peer`'s manifest was asked for in the last [`CHECK_EVERY`]:
    /// what came of it (the cached one, or none) stands until then.
    fn manifest_fresh(&self, peer: &Peer, now: u64) -> bool {
        let asked = self
            .manifests_at
            .lock()
            .ok()
            .and_then(|at| at.get(&peer.peer_id).copied());
        within(asked, now, CHECK_EVERY)
    }

    /// Forget what is known in memory of `peer_id`.
    fn drop_memory(&self, peer_id: &str) {
        for map in [&self.manifests_at, &self.pinged_at] {
            if let Ok(mut map) = map.lock() {
                map.remove(peer_id);
            }
        }
    }

    /// Bring what is known of `peer` up to date in the background, unless it
    /// is current or a refresh of it already runs: a ping when its
    /// reachability is unknown, then its manifest when that is stale.
    pub(crate) fn refresh_later(&self, state: &AppState, peer: Peer) {
        let now = now_ms();
        let current = match self.reachability(&peer, now) {
            None => false,
            Some(true) => self.manifest_fresh(&peer, now),
            Some(false) => true,
        };
        if current {
            return;
        }
        let started =
            (self.refreshing.lock()).is_ok_and(|mut running| running.insert(peer.peer_id.clone()));
        if !started {
            return;
        }
        /// Ends the refresh however its task ends.
        struct Running(Arc<PeerService>, String);
        impl Drop for Running {
            fn drop(&mut self) {
                if let Ok(mut running) = self.0.refreshing.lock() {
                    running.remove(&self.1);
                }
            }
        }
        let running = Running(state.peers.clone(), peer.peer_id.clone());
        let state = state.clone();
        tokio::spawn(async move {
            let service = running.0.clone();
            if let Some(peer) = service.checked(&state, peer).await {
                if peer.reachable(now_ms()) {
                    service.manifest_of(&peer).await;
                }
            }
            drop(running);
        });
    }

    /// `peer`, pinged first when its last answer is older than [`CHECK_EVERY`].
    /// A peer that no longer knows this gateway's token is forgotten (§10.10),
    /// its open mirrors failed, and answered as `None`.
    pub(crate) async fn checked(&self, state: &AppState, peer: Peer) -> Option<Peer> {
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
        // The public capability list is asked alongside: a check costs one timeout.
        let (pinged, capabilities) = tokio::join!(
            tokio::time::timeout(PING_TIMEOUT, client.ping()),
            tokio::time::timeout(PING_TIMEOUT, client.capabilities()),
        );
        if let Ok(mut at) = self.pinged_at.lock() {
            at.insert(peer.peer_id.clone(), now_ms());
        }
        match pinged {
            Ok(Ok(())) => {
                let capabilities = capabilities.ok().and_then(Result::ok);
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
                super::mirror::forget(state, &peer.peer_id).await;
                None
            }
            Ok(Err(_)) | Err(_) => Some(peer),
        }
    }

    /// `peer`'s manifest as `fleet.describe` shows it (its `GET /api/fleet/self`,
    /// compacted): asked for at most once per [`CHECK_EVERY`], answered or
    /// not, and cached; between asks, and when an ask fails, the cached one
    /// however old. `None`: none was ever fetched.
    pub(crate) async fn manifest_of(&self, peer: &Peer) -> Option<Value> {
        if self.manifest_fresh(peer, now_ms()) {
            return peer.manifest.clone();
        }
        if let Ok(mut at) = self.manifests_at.lock() {
            at.insert(peer.peer_id.clone(), now_ms());
        }
        let client = PeerClient::for_peer(peer).ok()?;
        let fetch = client.json(Method::GET, "/api/fleet/self", None);
        let Ok(Ok(full)) = tokio::time::timeout(MANIFEST_TIMEOUT, fetch).await else {
            return peer.manifest.clone();
        };
        let manifest = super::manifest::compact_json(full);
        let now = now_ms();
        if let Err(error) = self
            .store
            .seen(&peer.peer_id, now, None, Some(manifest.clone()))
            .await
        {
            tracing::warn!(peer = %peer.peer_id, "failed to keep a peer's manifest: {error:#}");
        }
        self.presence.seen(&peer.peer_id, Some(now));
        Some(manifest)
    }

    /// Keep `peer` as if it had been linked; a manifest it carries counts as
    /// fetched when it was last seen.
    #[cfg(test)]
    pub(crate) async fn store_for_test(&self, peer: Peer) {
        if let (Some(_), Some(seen)) = (&peer.manifest, peer.last_seen_ms) {
            self.manifests_at
                .lock()
                .unwrap()
                .insert(peer.peer_id.clone(), seen);
        }
        self.presence.seen(&peer.peer_id, peer.last_seen_ms);
        self.store.put(&peer).await.unwrap();
        self.refresh_names().await;
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
                PeerError::Unreachable(_) | PeerError::NoAnswer(_) => LinkError::Unreachable,
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
        if peers.iter().any(|other| same_name(&other.name, &peer.name)) {
            return Err(LinkError::NameTaken);
        }
        // The first authenticated call proves the new credentials (and, sealed,
        // the transport key) before they are kept; the public capability list
        // fills the cache.
        let client = PeerClient::for_peer(&peer).map_err(LinkError::Store)?;
        client.ping().await.map_err(|error| match error {
            PeerError::Unreachable(_) | PeerError::NoAnswer(_) => LinkError::Unreachable,
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
    /// effort; take back its own login here too, so one call ends both
    /// directions. The link is over either way, and its open mirrors end as
    /// lost. `false`: no such peer.
    pub(crate) async fn unlink(&self, state: &AppState, peer_id: &str) -> Option<Unlinked> {
        let peer = self.get(peer_id).await?;
        let remote_revoked = revoke(&peer).await;
        let (counterpart_found, counterpart_revoked) = revoke_login(state, &peer.server_id);
        super::mirror::lose(state, peer_id, &peer.name).await;
        let forgotten = match self.store.remove(peer_id).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(peer = %peer_id, "failed to forget an unlinked peer: {error:#}");
                false
            }
        };
        self.presence.gone(peer_id);
        self.drop_memory(peer_id);
        self.refresh_names().await;
        self.sockets.stop(peer_id);
        Some(Unlinked {
            remote_revoked,
            counterpart_found,
            counterpart_revoked,
            forgotten,
        })
    }

    /// Call `peer_id` `name` here (§10.16). `None`: no such peer.
    pub(crate) async fn rename(
        &self,
        peer_id: &str,
        name: &str,
    ) -> Result<Option<Peer>, RenameError> {
        let name = name.trim();
        if name.is_empty() || !crate::valid_device_name(name) || reserved_name(name) {
            return Err(RenameError::Invalid);
        }
        let peers = self.store.list().await.map_err(RenameError::Store)?;
        if peers
            .iter()
            .any(|peer| peer.peer_id != peer_id && same_name(&peer.name, name))
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

    /// Drop the row only (the peer answered 401/403: it no longer knows us).
    /// Its login here stays: it may still be linked to this computer. Called
    /// through [`mirror::forget`](super::mirror::forget), which ends its
    /// open mirrors first.
    pub(super) async fn forget(&self, peer_id: &str) {
        if let Err(error) = self.store.remove(peer_id).await {
            tracing::warn!(peer = %peer_id, "failed to forget a peer: {error:#}");
        }
        self.presence.gone(peer_id);
        self.drop_memory(peer_id);
        self.refresh_names().await;
        // Last: the caller may be the follower itself.
        self.sockets.stop(peer_id);
    }

    /// It answered just now. Kept in the store at most every
    /// [`SEEN_WRITE_EVERY`]: a socket's frames come far more often.
    pub(crate) async fn mark_seen(&self, peer_id: &str) {
        let now = now_ms();
        if let (_, Some(seen)) = self.presence.of(peer_id, now) {
            if now.saturating_sub(seen) < SEEN_WRITE_EVERY.as_millis() as u64 {
                return;
            }
        }
        if let Err(error) = self.store.seen(peer_id, now, None, None).await {
            tracing::warn!(peer = %peer_id, "failed to record a peer's answer: {error:#}");
        }
        self.presence.seen(peer_id, Some(now));
    }

    /// The peers' names: `machine_unknown`'s list, and the `mentions` a
    /// task's `tasks.read` lists (G5).
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
