//! Asset indexing and the file half of the unified content model.
//!
//! An asset is a file a session's workspaces produced that the user may want to
//! look at on the phone. Two feeds keep the index current, in this order:
//!
//! 1. Herdr's `worktree.*` events, which the gateway already subscribes to.
//!    They carry the checkout's root path (and its workspace), never the files
//!    written inside it -- protocol 17 has no per-file event -- so what they
//!    give is a precise "this root just changed" trigger, and the files come
//!    from scanning exactly that root at that moment.
//! 2. An mtime scan of the session's workspace roots, which is what a cold start
//!    or a plain listing uses, and what makes the endpoint work on a session
//!    that never touched a worktree.
//!
//! Reading a file is gated on provenance. The first rule is the one that has
//! always applied: the path canonicalizes to a regular file inside a root the
//! session currently has. The second rule exists because a workspace closes
//! while the file it produced is still the thing the user wants to look at --
//! a removed worktree used to take a twelve-hour-old asset with it. So when the
//! roots no longer contain the path, an entry that *was* indexed while its root
//! was live is still served, and only ever by replaying its stored canonical
//! path: it is canonicalized again at read time and must come back byte-for-byte
//! equal, so a symlink swapped into the old location resolves elsewhere and
//! misses. Nothing that was never indexed becomes reachable by either rule, and
//! every failure of either is the same 404 as an unknown id.
//! ---------------------------------------------------------------------------

use std::collections::{HashMap, VecDeque};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use super::uploads::{sniff_upload_kind, uploads_dir, MAX_UPLOAD_NAME_CHARS};
use crate::{
    api_error, backend, content_envelope, find_session, require_device, terminal_backend,
    ApiResult, AppState, SessionConfig, CONTENT_SCHEMA_VERSION,
};

/// A phone previews artifacts, it does not download archives. Anything larger
/// is refused rather than streamed, so one request can never tie up the host.
pub(crate) const MAX_ASSET_CONTENT_BYTES: u64 = 10 * 1024 * 1024;
pub(crate) const ASSET_CONTENT_CHUNK_BYTES: usize = 64 * 1024;
/// Enough of a file's head to decide what it is. The same bytes settle both the
/// magic-number check and the "is this text" question.
pub(crate) const ASSET_SNIFF_BYTES: usize = 8 * 1024;
/// A workspace scan is a preview mechanism, not an indexer: it stays shallow,
/// stops at a fixed budget, and never descends into a dependency or build
/// directory. Depth 1 is the root's own children.
pub(crate) const ASSET_SCAN_MAX_DEPTH: usize = 4;
pub(crate) const ASSET_SCAN_MAX_ENTRIES: usize = 20_000;
pub(crate) const ASSET_SCAN_MAX_FILES: usize = 4_000;
pub(crate) const ASSET_LIST_DEFAULT_LIMIT: usize = 50;
pub(crate) const ASSET_LIST_MAX_LIMIT: usize = 200;
/// The index is a rolling window of what the workspaces produced recently;
/// the oldest entries are dropped so a long-lived gateway cannot grow without
/// end.
pub(crate) const MAX_INDEXED_ASSETS: usize = 4_000;
/// How many `(session, scope)` root listings the asset index remembers.
///
/// These are only a fallback: `session_asset_roots` uses them when the live
/// pane list has nothing for the scope, which means the tab or workspace has
/// closed. Evicting the oldest costs a long-closed tab its last-known roots
/// and nothing else, and the alternative was keeping every scope forever.
pub(crate) const MAX_REMEMBERED_ROOT_SCOPES: usize = 64;
/// A worktree event re-scans that root, but only files written around the event
/// are announced: the first scan of an old checkout is not "just created".
pub(crate) const ASSET_EVENT_MAX_AGE_MS: u128 = 10 * 60 * 1000;
pub(crate) const MAX_ASSET_EVENTS_PER_WORKTREE: usize = 20;
/// Directories holding dependencies, build output, or vendored code. None of it
/// is something an agent just produced for the user to look at, and all of it
/// is big enough to swamp a scan. Names starting with a dot are skipped
/// separately, which covers `.git`, `.venv`, `.next`, and friends.
pub(crate) const ASSET_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "vendor",
    "Pods",
    "DerivedData",
    "__pycache__",
    "venv",
    "coverage",
];

/// What a file is, decided from its bytes. The client picks a viewer from this,
/// so it is derived again on every read rather than trusted from a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetKind {
    Image,
    Markdown,
    Text,
    Pdf,
    Binary,
}

impl AssetKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AssetKind::Image => "image",
            AssetKind::Markdown => "markdown",
            AssetKind::Text => "text",
            AssetKind::Pdf => "pdf",
            AssetKind::Binary => "binary",
        }
    }

    /// Binary is the one kind with nothing to show, so it is the one kind the
    /// content endpoint refuses instead of streaming.
    pub(crate) fn previewable(self) -> bool {
        !matches!(self, AssetKind::Binary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AssetType {
    pub(crate) kind: AssetKind,
    pub(crate) mime: &'static str,
}

/// A directory a session works in, and the Herdr identifiers that explain where
/// it came from.
#[derive(Debug, Clone)]
pub(crate) struct AssetRoot {
    pub(crate) path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) workspace_id: Option<String>,
    pub(crate) tab_id: Option<String>,
    pub(crate) pane_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AssetEntry {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) modified_unix_ms: u128,
    pub(crate) root: PathBuf,
    pub(crate) session_id: String,
    pub(crate) workspace_id: Option<String>,
    pub(crate) tab_id: Option<String>,
    pub(crate) pane_id: Option<String>,
}

/// Which unit an assets request is scoped to.
///
/// tmux's tab is the tmux window, and is the granularity this exists to scope
/// to: a tmux *session* -- what this gateway calls a workspace -- spans every
/// project the developer happens to have a window open on, so scoping by
/// workspace narrows nothing on a machine with one tmux session (see
/// `tmux.rs:840-841`: `fields[0]` / the session is the workspace, `fields[1]`
/// / the window is the tab).
///
/// Herdr's tabs sit *inside* one of its own workspaces -- a herdr workspace's
/// `tab_count` need not be one -- and herdr's own workspace is already the
/// granularity card #802 scoped to. Narrowing a herdr session down to one tab
/// could hide a sibling tab's files that belong to the very same piece of
/// work, which would be a behavior change on a path herdr already had right
/// ("herdr must not change"). So a herdr session's tab id is resolved to the
/// workspace that owns it (`resolve_asset_scope`), and everything downstream
/// -- roots, the index, the cache -- scopes on that workspace instead of the
/// tab.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum AssetScope {
    Tab(String),
    Workspace(String),
}

impl AssetScope {
    pub(crate) fn matches_root(&self, root: &AssetRoot) -> bool {
        match self {
            AssetScope::Tab(id) => root.tab_id.as_deref() == Some(id.as_str()),
            AssetScope::Workspace(id) => root.workspace_id.as_deref() == Some(id.as_str()),
        }
    }

    pub(crate) fn matches_entry(&self, entry: &AssetEntry) -> bool {
        match self {
            AssetScope::Tab(id) => entry.tab_id.as_deref() == Some(id.as_str()),
            AssetScope::Workspace(id) => entry.workspace_id.as_deref() == Some(id.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScannedFile {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) modified_unix_ms: u128,
}

/// Everything the gateway knows about produced files, keyed by asset id. The
/// id is derived from the path, so the same file rediscovered by a later scan
/// -- or by a different root -- lands on the same entry instead of a duplicate.
#[derive(Debug, Default)]
pub(crate) struct AssetIndex {
    pub(crate) entries: HashMap<String, AssetEntry>,
    /// Keyed on `(session_id, scope)`. `scope` is `None` for the whole-session
    /// callers and `Some` for a scoped one, so the two never share a slot --
    /// see `session_asset_roots`.
    ///
    /// Bounded, because the key space is not. A tmux tab id is a window id,
    /// which counts up for the life of the tmux server and is never reused, so
    /// this took one permanent entry -- holding that tab's whole `Vec` of
    /// root paths -- for every tab whose assets anybody ever opened. Nothing
    /// removed from it: `forget_under` and `prune` both touch `entries` only.
    pub(crate) roots: HashMap<(String, Option<AssetScope>), Vec<AssetRoot>>,
    /// Insertion order for `roots`, so the cap evicts the oldest scope rather
    /// than whichever one the hasher happens to land on. An entry is here
    /// exactly while its key is in `roots`.
    pub(crate) roots_order: VecDeque<(String, Option<AssetScope>)>,
}

impl AssetIndex {
    /// Fold one scanned file in. Answers whether this path had not been seen
    /// before, which is what makes an `asset.created` event honest.
    pub(crate) fn upsert(&mut self, entry: AssetEntry) -> bool {
        match self.entries.get_mut(&entry.id) {
            Some(existing) => {
                existing.size = entry.size;
                existing.modified_unix_ms = entry.modified_unix_ms;
                existing.name = entry.name;
                // Workspace roots nest: `~/.ws` and `~/.ws/api` both see the
                // same file. The deeper root is the one that actually produced
                // it, so it wins the attribution.
                if entry.root.as_os_str().len() > existing.root.as_os_str().len() {
                    existing.root = entry.root;
                    existing.session_id = entry.session_id;
                    existing.workspace_id = entry.workspace_id;
                    existing.tab_id = entry.tab_id;
                    existing.pane_id = entry.pane_id;
                }
                false
            }
            None => {
                self.entries.insert(entry.id.clone(), entry);
                true
            }
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<AssetEntry> {
        self.entries.get(id).cloned()
    }

    pub(crate) fn remember_roots(
        &mut self,
        session_id: &str,
        scope: Option<&AssetScope>,
        roots: Vec<AssetRoot>,
    ) {
        let key = (session_id.to_owned(), scope.cloned());
        if self.roots.insert(key.clone(), roots).is_none() {
            self.roots_order.push_back(key);
        }
        while self.roots_order.len() > MAX_REMEMBERED_ROOT_SCOPES {
            if let Some(oldest) = self.roots_order.pop_front() {
                self.roots.remove(&oldest);
            }
        }
    }

    pub(crate) fn known_roots(
        &self,
        session_id: &str,
        scope: Option<&AssetScope>,
    ) -> Vec<AssetRoot> {
        self.roots
            .get(&(session_id.to_owned(), scope.cloned()))
            .cloned()
            .unwrap_or_default()
    }

    /// Newest first, cut to one page.
    pub(crate) fn session_assets(
        &self,
        session_id: &str,
        scope: &AssetScope,
        since_unix_ms: Option<u128>,
        limit: usize,
    ) -> Vec<AssetEntry> {
        let mut entries = self.session_assets_ordered(session_id, scope, since_unix_ms);
        entries.truncate(limit);
        entries
    }

    /// The same ordering with nothing dropped -- newest first, with the path as
    /// the tie-break so two files written in the same millisecond still order
    /// the same way on every call.
    ///
    /// The caller that wants this rather than a page is the one that has to
    /// look past the page to fill it: a `kind` filter cannot be answered from
    /// the index, because what a file is comes from its bytes.
    ///
    /// Filtered by `scope` as well as `session_id`: an entry ingested under
    /// this session from an unrelated tab or workspace -- whether from before
    /// this scoping existed, or from the cold-start reindex in `asset_content`,
    /// which still rebuilds a whole session's worth of roots -- must not leak
    /// into a listing scoped to one tab (or, for herdr, one workspace). An
    /// entry with neither a workspace_id nor a tab_id (the exact-path lookup
    /// can index one without ever calling `pane_list_roots`) never matches a
    /// specific scope either, for the same reason: an unattributed file is not
    /// known to belong here.
    pub(crate) fn session_assets_ordered(
        &self,
        session_id: &str,
        scope: &AssetScope,
        since_unix_ms: Option<u128>,
    ) -> Vec<AssetEntry> {
        let mut entries: Vec<AssetEntry> = self
            .entries
            .values()
            .filter(|entry| entry.session_id == session_id)
            .filter(|entry| scope.matches_entry(entry))
            .filter(|entry| match since_unix_ms {
                Some(since) => entry.modified_unix_ms > since,
                None => true,
            })
            .cloned()
            .collect();
        entries.sort_by(|left, right| {
            right
                .modified_unix_ms
                .cmp(&left.modified_unix_ms)
                .then_with(|| left.path.cmp(&right.path))
        });
        entries
    }

    /// A removed worktree takes its files with it: keeping them would hand out
    /// ids that can only ever 404.
    pub(crate) fn forget_under(&mut self, root: &FsPath) {
        self.entries
            .retain(|_, entry| !entry.path.starts_with(root));
    }

    /// Keep the index a rolling window over what was produced recently.
    pub(crate) fn prune(&mut self) {
        if self.entries.len() <= MAX_INDEXED_ASSETS {
            return;
        }
        let mut ordered: Vec<(String, u128)> = self
            .entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.modified_unix_ms))
            .collect();
        ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        for (id, _) in ordered.into_iter().skip(MAX_INDEXED_ASSETS) {
            self.entries.remove(&id);
        }
    }
}

/// Opaque to the client, stable for the gateway: the same file keeps its id
/// across scans and across restarts, and the id itself carries no path a caller
/// could bend into something else.
pub(crate) fn asset_id(path: &FsPath) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let mut id = String::from("as_");
    for byte in digest.iter().take(12) {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

pub(crate) fn system_time_unix_ms(time: Option<SystemTime>) -> u128 {
    time.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// Decide what a file is from its first bytes. Images and PDFs are recognised
/// by magic number; everything else has to look like text before it can be one
/// of the text kinds, and only then does the extension get a say -- it decides
/// markdown from plain text and nothing else, so a `.md` full of binary is
/// still binary.
pub(crate) fn sniff_asset_type(bytes: &[u8], name: &str) -> AssetType {
    if let Some(image) = sniff_upload_kind(bytes) {
        return AssetType {
            kind: AssetKind::Image,
            mime: image.mime,
        };
    }
    if bytes.starts_with(b"%PDF-") {
        return AssetType {
            kind: AssetKind::Pdf,
            mime: "application/pdf",
        };
    }
    if !looks_textual(bytes) {
        return AssetType {
            kind: AssetKind::Binary,
            mime: "application/octet-stream",
        };
    }
    if has_markdown_extension(name) {
        return AssetType {
            kind: AssetKind::Markdown,
            mime: "text/markdown; charset=utf-8",
        };
    }
    AssetType {
        kind: AssetKind::Text,
        mime: "text/plain; charset=utf-8",
    }
}

pub(crate) fn has_markdown_extension(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .is_some_and(|extension| matches!(extension.as_str(), "md" | "markdown" | "mdx"))
}

/// Text is UTF-8 without NUL bytes and without a crowd of control characters.
/// The probe is a prefix of the file, so a multi-byte character cut in half at
/// the end is not held against it; an invalid sequence anywhere else is.
pub(crate) fn looks_textual(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0) {
        return false;
    }
    let valid_up_to = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(err) if err.error_len().is_none() => err.valid_up_to(),
        Err(_) => return false,
    };
    let text = &bytes[..valid_up_to];
    if text.is_empty() {
        return false;
    }
    let control = text
        .iter()
        .filter(|byte| **byte < 0x20 && !matches!(**byte, b'\t' | b'\n' | b'\r' | 0x0c))
        .count();
    control * 20 <= text.len()
}

/// A pane sitting at the filesystem root, or straight in the home directory, is
/// not a workspace: scanning from there would sweep the whole machine.
pub(crate) fn is_scannable_root(path: &FsPath) -> bool {
    if !path.is_absolute() {
        return false;
    }
    if path.components().count() < 3 {
        return false;
    }
    if dirs::home_dir().is_some_and(|home| path == home) {
        return false;
    }
    true
}

/// Herdr does not report a "workspace root" as such. What it does report, for
/// every pane, is the directory that pane runs in -- which is where an agent
/// working in that pane writes its output.
pub(crate) fn pane_list_roots(session_id: &str, response: &Value) -> Vec<AssetRoot> {
    let mut roots: Vec<AssetRoot> = Vec::new();
    let Some(panes) = response.pointer("/result/panes").and_then(Value::as_array) else {
        return roots;
    };
    for pane in panes {
        let Some(cwd) = pane
            .get("cwd")
            .and_then(Value::as_str)
            .or_else(|| pane.get("foreground_cwd").and_then(Value::as_str))
        else {
            continue;
        };
        let path = PathBuf::from(cwd);
        if !is_scannable_root(&path) {
            continue;
        }
        if roots.iter().any(|root| root.path == path) {
            continue;
        }
        roots.push(AssetRoot {
            path,
            session_id: session_id.to_owned(),
            workspace_id: pane
                .get("workspace_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tab_id: pane
                .get("tab_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pane_id: pane
                .get("pane_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    roots
}

/// Herdr's `worktree.*` events carry the checkout root and its workspace, not
/// the files written inside it. Treated as a trigger rather than as content,
/// they are still the precise signal the asset index wants: scan this root, now.
pub(crate) fn worktree_event_root(session_id: &str, line: &str) -> Option<AssetRoot> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let event = value.get("event").and_then(Value::as_str)?;
    if !matches!(event, "worktree_created" | "worktree_opened") {
        return None;
    }
    let path = PathBuf::from(
        value
            .pointer("/data/worktree/path")
            .and_then(Value::as_str)?,
    );
    if !is_scannable_root(&path) {
        return None;
    }
    Some(AssetRoot {
        path,
        session_id: session_id.to_owned(),
        workspace_id: value
            .pointer("/data/workspace/workspace_id")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/data/workspace_id").and_then(Value::as_str))
            .map(str::to_owned),
        // The event carries the checkout's workspace, never a tab -- herdr's
        // own worktree.* payloads have no tab in them. Scoping never needs it:
        // a herdr session always resolves a request's tab to its workspace
        // (see `AssetScope`), and a tmux session never produces this event in
        // the first place (tmux worktrees are made through the git fallback,
        // with no protocol event of their own).
        tab_id: None,
        pane_id: None,
    })
}

pub(crate) fn worktree_event_removed_root(line: &str) -> Option<PathBuf> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("event").and_then(Value::as_str)? != "worktree_removed" {
        return None;
    }
    Some(PathBuf::from(
        value
            .pointer("/data/worktree/path")
            .and_then(Value::as_str)?,
    ))
}

/// Walk one root for files worth showing. Shallow, budgeted, and blind to
/// dependency directories, dot directories, and symlinks -- a link is how a
/// scan would leave the root, so it is never followed.
pub(crate) fn scan_workspace_root(
    root: &FsPath,
    max_depth: usize,
    max_files: usize,
) -> Vec<ScannedFile> {
    let mut files: Vec<ScannedFile> = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.flatten() {
            visited += 1;
            if visited > ASSET_SCAN_MAX_ENTRIES || files.len() >= max_files {
                return files;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth + 1 >= max_depth || ASSET_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files.push(ScannedFile {
                path: entry.path(),
                name,
                size: metadata.len(),
                modified_unix_ms: system_time_unix_ms(metadata.modified().ok()),
            });
        }
    }
    files
}

/// Scan one root and fold the result into the index. Answers with the entries
/// that were not there before, newest first.
pub(crate) fn ingest_root(index: &Mutex<AssetIndex>, root: &AssetRoot) -> Vec<AssetEntry> {
    let Ok(canonical) = std::fs::canonicalize(&root.path) else {
        return Vec::new();
    };
    let files = scan_workspace_root(&canonical, ASSET_SCAN_MAX_DEPTH, ASSET_SCAN_MAX_FILES);
    let Ok(mut index) = index.lock() else {
        tracing::warn!(
            "asset index lock failed while scanning {}",
            canonical.display()
        );
        return Vec::new();
    };
    let mut created: Vec<AssetEntry> = Vec::new();
    for file in files {
        let entry = AssetEntry {
            id: asset_id(&file.path),
            path: file.path,
            name: file.name,
            size: file.size,
            modified_unix_ms: file.modified_unix_ms,
            root: canonical.clone(),
            session_id: root.session_id.clone(),
            workspace_id: root.workspace_id.clone(),
            tab_id: root.tab_id.clone(),
            pane_id: root.pane_id.clone(),
        };
        if index.upsert(entry.clone()) {
            created.push(entry);
        }
    }
    index.prune();
    created.sort_by_key(|entry| std::cmp::Reverse(entry.modified_unix_ms));
    created
}

/// Scanning is blocking filesystem work, so it runs off the async runtime.
pub(crate) async fn ingest_roots(
    index: Arc<Mutex<AssetIndex>>,
    roots: Vec<AssetRoot>,
) -> Vec<AssetEntry> {
    tokio::task::spawn_blocking(move || {
        let mut created = Vec::new();
        for root in &roots {
            created.extend(ingest_root(&index, root));
        }
        created
    })
    .await
    .unwrap_or_default()
}

/// Herdr is the source of truth for where a session works, but a listing still
/// has to answer when the socket is down, so the last known roots are kept.
///
/// `scope` narrows the pane list tmux hands back -- every pane on the whole
/// server -- down to the one tab a caller asked about (or, for a herdr
/// session, the workspace that tab lives in; see `AssetScope`). `None` keeps
/// the old, whole-session answer for the callers that still want it (the
/// recent-directories picker, a task's candidate repo roots, and the
/// cold-start reindex), so nothing about them changes.
///
/// The cache is keyed on `(session_id, scope)`, not on `session_id` alone: a
/// scoped call and a whole-session call against the same session must not
/// read back each other's roots when `list_panes` fails and the last known
/// set is served instead. Collapsing the key back to `session_id` alone would
/// quietly widen every scoped listing back out to the whole machine the
/// moment the socket hiccuped -- the exact bug this function exists to close,
/// just deferred to the fallback path.
pub(crate) async fn session_asset_roots(
    state: &AppState,
    session: &SessionConfig,
    scope: Option<&AssetScope>,
) -> Vec<AssetRoot> {
    let response = terminal_backend(session)
        .list_panes()
        .await
        .map(backend::compat::pane_list)
        .map_err(|err| err.to_string());
    let mut fresh = match response {
        Ok(value) => pane_list_roots(&session.id, &value),
        Err(err) => {
            tracing::warn!("asset roots: pane.list failed: {err}");
            Vec::new()
        }
    };
    if let Some(scope) = scope {
        fresh.retain(|root| scope.matches_root(root));
    }
    if fresh.is_empty() {
        return match state.assets.lock() {
            Ok(index) => index.known_roots(&session.id, scope),
            Err(_) => Vec::new(),
        };
    }
    if let Ok(mut index) = state.assets.lock() {
        index.remember_roots(&session.id, scope, fresh.clone());
    }
    fresh
}

/// What a request's tab id actually scopes to, for this session's backend.
///
/// tmux: the tab id names a tmux window directly, which is exactly the unit
/// this card narrows to -- no lookup needed.
///
/// Herdr: herdr's own tabs sit inside one of its workspaces, and a herdr
/// workspace is the granularity that must not narrow (see `AssetScope`). The
/// tab id is translated to the workspace that owns it by asking the live pane
/// list which workspace that tab's panes belong to. A tab the pane list no
/// longer has -- a closed tab, or a socket hiccup -- still needs *some* scope
/// to key the cache and filter on, so the tab id itself is kept as the
/// fallback: this keeps the answer scoped (and therefore empty rather than
/// silently widened back to every workspace) even when the live lookup can't
/// resolve it.
pub(crate) async fn resolve_asset_scope(session: &SessionConfig, tab_id: &str) -> AssetScope {
    if !session.backend.is_herdr() {
        return AssetScope::Tab(tab_id.to_owned());
    }
    let workspace_id = terminal_backend(session)
        .list_panes()
        .await
        .ok()
        .and_then(|panes| {
            panes
                .into_iter()
                .find(|pane| pane.tab_id.as_str() == tab_id)
        })
        .map(|pane| pane.workspace_id.as_str().to_owned())
        .unwrap_or_else(|| tab_id.to_owned());
    AssetScope::Workspace(workspace_id)
}

pub(crate) fn canonical_roots(roots: &[AssetRoot]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(&root.path).ok())
        .collect()
}

/// The one gate on reading a file. Whatever an id claimed, the path has to
/// canonicalize to a regular file inside a root the session currently has.
/// Canonicalizing first is what closes symlink escapes: a link inside a root
/// that points outside it resolves to the outside path, and fails here.
pub(crate) fn resolve_asset_path(path: &FsPath, roots: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    roots
        .iter()
        .any(|root| canonical != *root && canonical.starts_with(root))
        .then_some(canonical)
}

/// What an id that is already in the index is allowed to read.
///
/// Root containment first, exactly as before: while the workspace that made the
/// file is still open, nothing about this changed. What is new is the second
/// answer, for the asset whose workspace has since closed -- a worktree removed
/// after the agent finished, which used to 404 the file it produced.
///
/// That fallback replays the entry's own stored canonical path and nothing
/// else. The path is canonicalized again and has to come back equal to what was
/// stored: a symlink dropped where the file used to be canonicalizes to its
/// target, which is a different path, and misses. A directory left in its place
/// is not a regular file, and misses. The file being gone at all misses. Since
/// the caller is an index lookup, a path that was never indexed has no entry to
/// replay and never reaches here -- provenance is what is being served, not a
/// filesystem.
pub(crate) fn resolve_indexed_asset_path(stored: &FsPath, roots: &[PathBuf]) -> Option<PathBuf> {
    if let Some(path) = resolve_asset_path(stored, roots) {
        return Some(path);
    }
    let canonical = std::fs::canonicalize(stored).ok()?;
    (canonical == stored && canonical.is_file()).then_some(canonical)
}

/// Resolve one exact path into an asset. The app needs this because a file
/// path printed in a terminal has to map to the file it names -- matching by
/// name against the listing would land on the wrong one. The path is held to
/// the same fence as every other read, and anything that fails it answers "no
/// match" rather than an error, so this cannot be used to probe the host.
///
/// A scan is not involved, so a file deeper or more obscure than the scan
/// bothers with still resolves: the user pointed at it.
/// A session's roots with the gateway's uploads directory appended, so a
/// lookup can answer for a file the gateway itself stored on the phone's
/// behalf. `None` -- a state dir that cannot be resolved -- leaves the roots
/// exactly as they were.
pub(crate) fn with_uploads_root(
    mut roots: Vec<AssetRoot>,
    session_id: &str,
    uploads: Option<PathBuf>,
) -> Vec<AssetRoot> {
    if let Some(path) = uploads {
        roots.push(AssetRoot {
            path,
            session_id: session_id.to_owned(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        });
    }
    roots
}

/// Extra roots are used ONLY for an explicit file lookup, never a directory
/// scan. Paired devices may explicitly open files in the gateway account's
/// home, including sibling projects and dotfiles. Platform home/cache/temp
/// paths are configuration, not terminal output.
pub(crate) fn preview_lookup_roots(
    mut roots: Vec<AssetRoot>,
    session_id: &str,
    home: Option<&FsPath>,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Vec<AssetRoot> {
    let canonical_home = home.and_then(|path| std::fs::canonicalize(path).ok());
    if let Some(path) = canonical_home.as_ref().filter(|path| {
        path.is_dir() && path.parent().is_some() && !roots.iter().any(|root| root.path == **path)
    }) {
        roots.push(AssetRoot {
            path: path.clone(),
            session_id: session_id.to_owned(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        });
    }
    for candidate in candidates {
        let Ok(path) = std::fs::canonicalize(candidate) else {
            continue;
        };
        // Cache/temp configuration must not widen access above the account
        // home or to the filesystem root. Resolve aliases such as macOS /tmp first.
        if !path.is_dir()
            || path.parent().is_none()
            || canonical_home
                .as_ref()
                .is_some_and(|home| home.starts_with(&path))
            || roots.iter().any(|root| root.path == path)
        {
            continue;
        }
        roots.push(AssetRoot {
            path,
            session_id: session_id.to_owned(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        });
    }
    roots
}

pub(crate) fn asset_entry_for_path(raw: &str, roots: &[AssetRoot]) -> Option<AssetEntry> {
    let path = std::fs::canonicalize(raw).ok()?;
    if !path.is_file() {
        return None;
    }
    // Workspace roots nest, so the deepest containing root owns the file.
    let (owner, root) = roots
        .iter()
        .filter_map(|root| {
            std::fs::canonicalize(&root.path)
                .ok()
                .map(|canonical| (root, canonical))
        })
        .filter(|(_, canonical)| path != *canonical && path.starts_with(canonical))
        .max_by_key(|(_, canonical)| canonical.as_os_str().len())?;
    let metadata = std::fs::metadata(&path).ok()?;
    Some(AssetEntry {
        id: asset_id(&path),
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        path,
        size: metadata.len(),
        modified_unix_ms: system_time_unix_ms(metadata.modified().ok()),
        root,
        session_id: owner.session_id.clone(),
        workspace_id: owner.workspace_id.clone(),
        tab_id: owner.tab_id.clone(),
        pane_id: owner.pane_id.clone(),
    })
}

/// Read only as much of a file as it takes to type it.
pub(crate) fn read_asset_head(path: &FsPath) -> Vec<u8> {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut head = Vec::new();
    if file
        .take(ASSET_SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return Vec::new();
    }
    head
}

pub(crate) fn asset_json(entry: &AssetEntry, asset_type: AssetType) -> Value {
    json!({
        "id": entry.id,
        "path": entry.path.to_string_lossy(),
        "name": entry.name,
        "kind": asset_type.kind.as_str(),
        "mime": asset_type.mime,
        "size": entry.size,
        "modified_unix_ms": entry.modified_unix_ms,
        "origin": {
            "session_id": entry.session_id,
            "workspace_id": entry.workspace_id,
            "pane_id": entry.pane_id,
            "root": entry.root.to_string_lossy(),
        },
        "previewable": asset_type.kind.previewable(),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssetsQuery {
    /// Unix milliseconds, matching every `modified_unix_ms` on the wire; only
    /// files modified strictly after this are returned, so a client can poll
    /// for what is new without re-reading the list.
    #[serde(default)]
    pub(crate) since: Option<u64>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    /// Comma-separated allow-list of asset kinds, e.g. `markdown,pdf`. Absent
    /// or empty means every kind, so an old client is unaffected.
    #[serde(default)]
    pub(crate) kind: Option<String>,
    /// One absolute path to resolve exactly, for a file path the user tapped in
    /// terminal output. Takes precedence over `since` and `limit`, and answers
    /// with either the one asset or none.
    #[serde(default)]
    pub(crate) path: Option<String>,
}

/// The `kind=` allow-list, normalized.
///
/// Comma-separated rather than one kind per request because a client's filters
/// do not map one to one onto the taxonomy -- a "documents" filter is markdown
/// and pdf -- and asking for both at once keeps "newest first" one ordering
/// instead of two lists the client has to merge and re-cut.
///
/// The values are the same strings an asset carries as its `kind`, so what can
/// be asked for is exactly what can be read back. A value that is not one of
/// them matches nothing, the way an unknown name in the events `types=`
/// allow-list matches nothing: the request is answered rather than refused, and
/// the applied list is echoed back so a client can see what it asked for.
pub(crate) fn asset_kind_filter(kind: Option<&str>) -> Vec<String> {
    kind.unwrap_or_default()
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Build one page of assets, newest first, sniffing each candidate as it goes.
///
/// The kinds allow-list is applied while walking rather than to a page already
/// cut, which is the whole point of it: `kind=image&limit=50` answers with the
/// 50 newest images, not with the images among the 50 newest files. A session
/// whose agent is editing source code writes source files faster than it writes
/// artifacts, so the second reading returns an empty list on a workspace that
/// is full of images.
///
/// Sniffing is the cost here, so a candidate is read once and the walk stops
/// the moment the page is full. Without a filter the candidates are already the
/// page, which is the old work and the old cost exactly.
pub(crate) fn asset_page(entries: Vec<AssetEntry>, kinds: &[String], limit: usize) -> Vec<Value> {
    let mut page: Vec<Value> = Vec::new();
    for entry in entries {
        if page.len() >= limit {
            break;
        }
        let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
        if !kinds.is_empty() && !kinds.iter().any(|kind| kind == asset_type.kind.as_str()) {
            continue;
        }
        page.push(asset_json(&entry, asset_type));
    }
    page
}

/// List what one tab produced recently, newest first.
///
/// Scoped to a tab rather than to a session or a workspace: with the tmux
/// backend a session is the whole tmux server and a workspace is a tmux
/// session -- one `tmux new-session`, which is commonly one long-running
/// `Work` session with a window per project -- so either one still pools
/// every project anyone happens to have open in that session. An agent's
/// Files sheet asks "what did this piece of work touch", which is the tab
/// (the tmux window) it is showing, not every tab the workspace happens to
/// contain. `tab_id` is a wire id exactly like a pane id on the other
/// handlers: the client already holds it from whatever pane or agent view
/// opened this sheet, and it is compared as-is against the wire-form `tab_id`
/// `list_panes()` already hands back, with no separate decode step needed
/// because both sides went through the same `TmuxWireIds` seam.
///
/// A herdr session's tab id is translated to its owning workspace before any
/// of this scoping happens (`resolve_asset_scope`), because herdr's own tabs
/// sit inside a workspace and that workspace is the granularity herdr must
/// keep -- see `AssetScope`.
pub(crate) async fn session_assets(
    State(state): State<AppState>,
    Path((session_id, tab_id)): Path<(String, String)>,
    Query(query): Query<AssetsQuery>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let session = find_session(&state.config, &session_id)?.clone();
    let scope = resolve_asset_scope(&session, &tab_id).await;
    let roots = session_asset_roots(&state, &session, Some(&scope)).await;

    // An exact path lookup asks about one file, so it neither waits for a scan
    // nor pages: it answers with that file or with nothing.
    if let Some(wanted) = query.path.clone() {
        // The uploads directory rides along as one more root, the same fold-in
        // `asset_content`'s cold start does. An upload's path is printed into
        // the pane and handed back by the upload response, but the file lives
        // in the gateway's own directory, never under a pane's cwd -- so
        // without this, the one path the client is guaranteed to hold is the
        // one path the lookup could never answer. Serving it widens nothing:
        // the directory is the gateway's own.
        let lookup_roots = with_uploads_root(roots.clone(), &session_id, uploads_dir().ok());
        let home = dirs::home_dir();
        let mut candidates = vec![std::env::temp_dir(), PathBuf::from("/tmp")];
        candidates.extend(dirs::cache_dir());
        candidates.extend(home.as_ref().map(|home| home.join(".cache")));
        let lookup_roots =
            preview_lookup_roots(lookup_roots, &session_id, home.as_deref(), candidates);
        // Expand only the current account's ~/ form, never shell syntax or
        // another account. Canonical containment is still checked below.
        let wanted = if let Some(relative) = wanted.strip_prefix("~/") {
            home.as_ref()
                .map(|home| home.join(relative).to_string_lossy().into_owned())
                .unwrap_or(wanted)
        } else {
            wanted
        };
        let entry = tokio::task::spawn_blocking(move || {
            let entry = asset_entry_for_path(&wanted, &lookup_roots)?;
            let asset_type = sniff_asset_type(&read_asset_head(&entry.path), &entry.name);
            Some((entry, asset_type))
        })
        .await
        .unwrap_or_default();
        let assets = match entry {
            Some((entry, asset_type)) => {
                // Remember it, so the id the client just received resolves at
                // the content endpoint even if no scan would have found it.
                lock_assets(&state)?.upsert(entry.clone());
                vec![asset_json(&entry, asset_type)]
            }
            None => Vec::new(),
        };
        return Ok(Json(content_envelope(json!({
            "session_id": session_id,
            "tab_id": tab_id,
            "assets": assets,
            "path": query.path,
        }))));
    }

    ingest_roots(state.assets.clone(), roots.clone()).await;

    let limit = query
        .limit
        .unwrap_or(ASSET_LIST_DEFAULT_LIMIT)
        .clamp(1, ASSET_LIST_MAX_LIMIT);
    let since = query.since.map(u128::from);
    let kinds = asset_kind_filter(query.kind.as_deref());
    // Unfiltered, the index cuts the page and only that page is sniffed:
    // metadata came from the scan, and reading a handful of file heads is cheap
    // where reading every file in the workspace would not be. A `kind` filter
    // has to be given the whole workspace in order instead, because the index
    // does not know what a file is -- its bytes do -- and the page has to be
    // filled from the newest matches rather than from whatever the newest
    // handful of files happened to be.
    let entries = {
        let index = lock_assets(&state)?;
        if kinds.is_empty() {
            index.session_assets(&session_id, &scope, since, limit)
        } else {
            index.session_assets_ordered(&session_id, &scope, since)
        }
    };
    let filter = kinds.clone();
    let assets = tokio::task::spawn_blocking(move || asset_page(entries, &filter, limit))
        .await
        .unwrap_or_default();

    Ok(Json(content_envelope(json!({
        "session_id": session_id,
        "tab_id": tab_id,
        "assets": assets,
        "limit": limit,
        "since": since.map(|since| since as u64),
        // The allow-list that was actually applied, empty when there was none.
        // Always present, so a client can tell a gateway that understands
        // `kind=` from an older one that ignored it.
        "kind": kinds,
        "roots": roots
            .iter()
            .map(|root| root.path.to_string_lossy())
            .collect::<Vec<_>>(),
    }))))
}

/// Stream one asset back, read-only.
pub(crate) async fn asset_content(
    State(state): State<AppState>,
    Path(asset_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;

    let mut entry = lock_assets(&state)?.get(&asset_id);
    // An image agent text embeds by path, which this gateway resolved for a
    // client and remembered with the session directory that fenced it. Read
    // against that directory again, now: the same canonicalize-then-contain
    // gate as every other read, so a file moved out or swapped for a symlink
    // out of the directory since is a 404.
    if entry.is_none() {
        if let Some(image) = state.message_images.served(&asset_id) {
            return serve_message_image(asset_id, image).await;
        }
    }
    if entry.is_none() {
        // Cold start: the app may hold an id from before a restart, so rebuild
        // the index from the live sessions once before answering. Uploads
        // are not under any pane's cwd -- they are the gateway's own
        // directory, not a workspace one -- so a rebuild from pane roots
        // alone would never see an upload made before the restart that just
        // emptied the index, even though the file is still on disk and
        // `resolve_indexed_asset_path` would happily serve it once the entry
        // exists. It is folded in here as one more root per session, which
        // widens nothing the gateway was not already willing to serve: the
        // uploads directory is its own, not a workspace's.
        let uploads = uploads_dir().ok();
        for session in state.config.sessions.clone() {
            let mut roots = session_asset_roots(&state, &session, None).await;
            if let Some(uploads) = &uploads {
                roots.push(AssetRoot {
                    path: uploads.clone(),
                    session_id: session.id.clone(),
                    workspace_id: None,
                    tab_id: None,
                    pane_id: None,
                });
            }
            ingest_roots(state.assets.clone(), roots).await;
        }
        entry = lock_assets(&state)?.get(&asset_id);
    }
    let Some(entry) = entry else {
        return Err(asset_not_found());
    };

    // Roots are resolved again rather than trusted from the index: what is
    // inside a workspace now is what decides. When it decides nothing -- the
    // workspace closed -- the entry's own stored path answers instead, under
    // the equality guard in `resolve_indexed_asset_path`.
    let session = find_session(&state.config, &entry.session_id)
        .map_err(|_| asset_not_found())?
        .clone();
    let roots = canonical_roots(&session_asset_roots(&state, &session, None).await);
    let entry_path = entry.path.clone();
    let Some(path) =
        tokio::task::spawn_blocking(move || resolve_indexed_asset_path(&entry_path, &roots))
            .await
            .unwrap_or_default()
    else {
        return Err(asset_not_found());
    };

    stream_asset(entry, path).await
}

/// `asset_content` for an image named in agent text. See `message_images`.
async fn serve_message_image(
    asset_id: String,
    image: crate::agents::message_images::ServedImage,
) -> ApiResult<Response> {
    let root = image.root.clone();
    let stored = image.path.clone();
    let Some(path) = tokio::task::spawn_blocking(move || resolve_asset_path(&stored, &[root]))
        .await
        .unwrap_or_default()
    else {
        return Err(asset_not_found());
    };
    let entry = AssetEntry {
        id: asset_id,
        name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        path: path.clone(),
        size: 0,
        modified_unix_ms: 0,
        root: image.root,
        session_id: String::new(),
        workspace_id: None,
        tab_id: None,
        pane_id: None,
    };
    stream_asset(entry, path).await
}

/// The read itself, once a path has passed its gate: size cap, sniff, stream.
async fn stream_asset(entry: AssetEntry, path: PathBuf) -> ApiResult<Response> {
    let metadata = std::fs::metadata(&path).map_err(|_| asset_not_found())?;
    if metadata.len() > MAX_ASSET_CONTENT_BYTES {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "asset_too_large",
            "the asset is larger than 10 MiB",
        ));
    }

    let sniff_path = path.clone();
    let name = entry.name.clone();
    let asset_type =
        tokio::task::spawn_blocking(move || sniff_asset_type(&read_asset_head(&sniff_path), &name))
            .await
            .map_err(|_| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "asset_read_failed",
                    "failed to read the asset",
                )
            })?;

    let mut entry = entry;
    entry.size = metadata.len();
    entry.modified_unix_ms = system_time_unix_ms(metadata.modified().ok());
    if !asset_type.kind.previewable() {
        return Ok((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": {
                    "code": "asset_not_previewable",
                    "message": "this asset has no preview; only its metadata is returned",
                },
                "asset": asset_json(&entry, asset_type),
            })),
        )
            .into_response());
    }

    let file = tokio::fs::File::open(&path).await.map_err(|err| {
        tracing::warn!("failed to open asset {}: {err}", path.display());
        asset_not_found()
    })?;
    let stream = async_stream::stream! {
        let mut file = file;
        let mut buffer = vec![0u8; ASSET_CONTENT_CHUNK_BYTES];
        loop {
            match tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await {
                Ok(0) => break,
                Ok(read) => yield Ok::<_, std::io::Error>(
                    axum::body::Bytes::copy_from_slice(&buffer[..read]),
                ),
                Err(err) => {
                    yield Err(err);
                    break;
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", asset_type.mime)
        .header("content-length", metadata.len())
        .header(
            "content-disposition",
            format!("inline; filename=\"{}\"", header_safe_name(&entry.name)),
        )
        .header("x-asset-kind", asset_type.kind.as_str())
        .header("x-content-schema-version", CONTENT_SCHEMA_VERSION)
        .body(Body::from_stream(stream))
        .map_err(|err| {
            tracing::warn!("failed to build asset response: {err}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "asset_read_failed",
                "failed to read the asset",
            )
        })
}

/// One answer for "no such asset" and for "that path is not inside a workspace
/// root": a caller must not be able to tell the two apart and map the host.
pub(crate) fn asset_not_found() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::NOT_FOUND,
        "asset_not_found",
        "asset not found in a session workspace",
    )
}

pub(crate) fn lock_assets(state: &AppState) -> ApiResult<std::sync::MutexGuard<'_, AssetIndex>> {
    state.assets.lock().map_err(|_| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "asset_lock_failed",
            "failed to lock the asset index",
        )
    })
}

/// A file name only ever reaches a header after being reduced to characters
/// that cannot end a quoted string or start a new header line.
pub(crate) fn header_safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .take(MAX_UPLOAD_NAME_CHARS)
        .collect();
    let safe = safe.trim().to_string();
    if safe.is_empty() {
        return String::from("asset");
    }
    safe
}

/// The SSE payload for a newly produced file, in the same versioned envelope
/// the asset endpoints answer with.
pub(crate) fn asset_created_payload(entry: &AssetEntry, asset_type: AssetType) -> String {
    content_envelope(json!({ "asset": asset_json(entry, asset_type) })).to_string()
}

/// The two asset routes: listing one tab's files, and serving their content.
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/sessions/{session_id}/tabs/{tab_id}/assets",
            get(session_assets),
        )
        .route("/api/assets/{asset_id}/content", get(asset_content))
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn every_pane_in_one_checkout_resolves_to_its_directory() {
        // Two panes in the same directory. The asset roots keep one entry for
        // it, under the first pane's id; the second pane must still be found.
        let list = json!({ "result": { "panes": [
            { "pane_id": "w1:p1", "cwd": "/work/team/app" },
            { "pane_id": "w1:p2", "cwd": "/work/team/app" },
            { "pane_id": "w1:p3", "foreground_cwd": "/work/team/api" },
            { "pane_id": "w1:p4", "cwd": "/" },
        ] } });
        assert_eq!(pane_list_roots("s", &list).len(), 2);
        assert_eq!(
            pane_cwd_in_list(&list, "w1:p1"),
            Some(PathBuf::from("/work/team/app"))
        );
        assert_eq!(
            pane_cwd_in_list(&list, "w1:p2"),
            Some(PathBuf::from("/work/team/app"))
        );
        assert_eq!(
            pane_cwd_in_list(&list, "w1:p3"),
            Some(PathBuf::from("/work/team/api"))
        );
        // Outside the fence, and unknown: no directory, so no git is run.
        assert_eq!(pane_cwd_in_list(&list, "w1:p4"), None);
        assert_eq!(pane_cwd_in_list(&list, "w9:p9"), None);
    }

    #[test]
    fn asset_reads_are_fenced_inside_the_workspace_roots() {
        let root = asset_test_dir("fence");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join("docs/report.md"), b"# report\n").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        let roots = vec![workspace.clone()];

        assert!(resolve_asset_path(&workspace.join("docs/report.md"), &roots).is_some());

        // Traversal out of the root, whatever shape it arrives in.
        assert!(
            resolve_asset_path(&workspace.join("docs/../../outside/secret.txt"), &roots).is_none()
        );
        assert!(resolve_asset_path(&outside.join("secret.txt"), &roots).is_none());
        assert!(resolve_asset_path(FsPath::new("/etc/hosts"), &roots).is_none());

        // A sibling whose name merely starts with the root's is not inside it.
        let neighbour = root.join("workspace-notes");
        std::fs::create_dir_all(&neighbour).unwrap();
        std::fs::write(neighbour.join("note.txt"), b"note\n").unwrap();
        assert!(resolve_asset_path(&neighbour.join("note.txt"), &roots).is_none());

        // A directory is not an asset, and neither is the root itself.
        assert!(resolve_asset_path(&workspace, &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("docs"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("missing.md"), &roots).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_indexed_asset_outlives_the_workspace_that_made_it() {
        // The worktree an agent wrote into was removed hours later. The roots
        // resolve to nothing now, which is the whole of what changed: the file
        // is still there and still the thing the user tapped.
        let root = asset_test_dir("provenance");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let report = workspace.join("report.md");
        std::fs::write(&report, b"# report\n").unwrap();

        // While the workspace is a root, nothing about the read changed.
        let live = vec![workspace.clone()];
        assert_eq!(
            resolve_indexed_asset_path(&report, &live),
            Some(report.clone())
        );

        // With no roots at all -- the workspace closed -- the stored path is
        // replayed and answers the same bytes.
        assert_eq!(
            resolve_indexed_asset_path(&report, &[]),
            Some(report.clone())
        );

        // A file that is gone is gone, roots or no roots.
        let deleted = workspace.join("gone.md");
        std::fs::write(&deleted, b"bye\n").unwrap();
        std::fs::remove_file(&deleted).unwrap();
        assert!(resolve_indexed_asset_path(&deleted, &[]).is_none());

        // A directory left where the file was is not a file.
        std::fs::create_dir_all(workspace.join("was-a-file")).unwrap();
        assert!(resolve_indexed_asset_path(&workspace.join("was-a-file"), &[]).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_swapped_into_a_stored_asset_path_is_not_that_asset() {
        // The attack the equality guard exists for: the workspace closes, the
        // real file is replaced by a link to somewhere the gateway would never
        // have indexed, and the old id is presented again.
        let root = asset_test_dir("provenance-symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        let stored = workspace.join("report.md");
        std::fs::write(&stored, b"# report\n").unwrap();
        assert!(resolve_indexed_asset_path(&stored, &[]).is_some());

        std::fs::remove_file(&stored).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), &stored).unwrap();

        // The path canonicalizes to the link's target, which is not the path
        // that was stored, so the replay refuses it -- and refuses it the same
        // way an unknown id is refused.
        assert!(resolve_indexed_asset_path(&stored, &[]).is_none());
        assert!(resolve_indexed_asset_path(&stored, std::slice::from_ref(&workspace)).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_path_that_was_never_indexed_has_no_entry_to_replay() {
        // The fallback is reached through an index lookup and nowhere else, so
        // provenance is what it serves. A file the gateway never scanned has no
        // entry, and the read stops at the lookup with the same 404 as a bad id.
        let root = asset_test_dir("provenance-unindexed");
        std::fs::create_dir_all(&root).unwrap();
        let indexed = root.join("indexed.md");
        let never = root.join("never-scanned.md");
        std::fs::write(&indexed, b"# indexed\n").unwrap();
        std::fs::write(&never, b"# private\n").unwrap();

        let mut index = AssetIndex::default();
        index.upsert(test_asset_entry(&indexed, &root, 1));

        assert!(index.get(&asset_id(&indexed)).is_some());
        assert!(index.get(&asset_id(&never)).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_uploads_directory_ingests_like_any_other_root_and_survives_a_cold_start() {
        // The bug this guards against: an upload lives in the gateway's own
        // uploads directory, which is not under any pane's cwd, so a rebuild
        // that only walks pane roots never finds it again once the in-memory
        // index is gone (e.g. after a restart). The cold-start path in
        // `asset_content` now adds the uploads directory as one more root per
        // session -- this proves that root ingests the same way a pane root
        // does, and that the resulting entry answers a lookup with none of the
        // session's live roots present, exactly the state a fresh process is
        // in right after startup.
        let uploads = asset_test_dir("uploads-cold-start");
        let upload = uploads.join("5969c1e3.webp");
        std::fs::write(&upload, b"fake webp bytes").unwrap();

        let root = AssetRoot {
            path: uploads.clone(),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        };
        let index: Mutex<AssetIndex> = Mutex::new(AssetIndex::default());
        let created = ingest_root(&index, &root);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].path, upload);
        assert_eq!(created[0].session_id, "default");

        // Simulate the state right after a restart answering `asset_content`:
        // the entry is in the index (just rebuilt), but the session's live
        // roots -- the pane cwds -- do not include the uploads directory and
        // never will. `resolve_indexed_asset_path`'s own-path fallback is what
        // actually serves it; this proves the entry it is given exists to
        // fall back on at all.
        let id = asset_id(&upload);
        let entry = index.lock().unwrap().get(&id).unwrap();
        assert_eq!(entry.path, upload);
        let no_live_pane_roots: Vec<PathBuf> = Vec::new();
        assert_eq!(
            resolve_indexed_asset_path(&entry.path, &no_live_pane_roots),
            Some(upload.clone())
        );

        std::fs::remove_dir_all(&uploads).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_workspace_root_cannot_be_read_or_scanned() {
        let root = asset_test_dir("symlink");
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), workspace.join("escape.txt"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape-dir")).unwrap();
        std::fs::write(workspace.join("own.txt"), b"mine\n").unwrap();
        let roots = vec![workspace.clone()];

        // The link resolves to where it points, which is outside the root.
        assert!(resolve_asset_path(&workspace.join("escape.txt"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("escape-dir/secret.txt"), &roots).is_none());
        assert!(resolve_asset_path(&workspace.join("own.txt"), &roots).is_some());

        // The scan never offers such a path in the first place.
        let names: Vec<String> = scan_workspace_root(&workspace, ASSET_SCAN_MAX_DEPTH, 100)
            .into_iter()
            .map(|file| file.name)
            .collect();
        assert_eq!(names, vec![String::from("own.txt")]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn asset_kind_comes_from_the_bytes_and_the_extension_only_splits_the_text_kinds() {
        assert_eq!(
            sniff_asset_type(&png_bytes(), "screenshot.png").kind,
            AssetKind::Image
        );
        // A name that lies about the content does not change what it is.
        assert_eq!(
            sniff_asset_type(&png_bytes(), "screenshot.md").kind,
            AssetKind::Image
        );
        assert_eq!(
            sniff_asset_type(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n", "report.pdf").kind,
            AssetKind::Pdf
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.md").kind,
            AssetKind::Markdown
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.MARKDOWN").kind,
            AssetKind::Markdown
        );
        assert_eq!(
            sniff_asset_type(b"# Title\n\nbody\n", "notes.txt").kind,
            AssetKind::Text
        );
        assert_eq!(
            sniff_asset_type("日本語とemoji 🎈\n".as_bytes(), "notes").kind,
            AssetKind::Text
        );
        // A markdown extension over bytes that are not text is still binary.
        assert_eq!(
            sniff_asset_type(b"\x00\x01\x02binary\x00", "notes.md").kind,
            AssetKind::Binary
        );
        assert_eq!(
            sniff_asset_type(&[0xff, 0xfe, 0xfd, 0xfc], "blob.bin").kind,
            AssetKind::Binary
        );
        assert_eq!(sniff_asset_type(b"", "empty.txt").kind, AssetKind::Text);

        assert!(AssetKind::Markdown.previewable());
        assert!(AssetKind::Image.previewable());
        assert!(AssetKind::Pdf.previewable());
        assert!(!AssetKind::Binary.previewable());

        // A UTF-8 character cut in half by the sniff window is a truncation,
        // not a binary file.
        let mut truncated = "héllo".as_bytes().to_vec();
        truncated.pop();
        assert!(looks_textual(&truncated));
        assert!(!looks_textual(b"text\x00text"));
    }

    #[test]
    fn a_scan_stays_shallow_and_skips_heavy_directories() {
        let root = asset_test_dir("scan");
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
        std::fs::write(root.join("report.md"), b"# report\n").unwrap();
        std::fs::write(root.join(".hidden"), b"hidden\n").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), b"module\n").unwrap();
        std::fs::write(root.join("target/debug/binary"), b"binary\n").unwrap();
        std::fs::write(root.join(".git/objects/blob"), b"blob\n").unwrap();
        std::fs::write(root.join("a/b/c/deep.txt"), b"deep\n").unwrap();
        std::fs::write(root.join("a/b/c/d/e/too-deep.txt"), b"too deep\n").unwrap();

        let mut names: Vec<String> = scan_workspace_root(&root, ASSET_SCAN_MAX_DEPTH, 100)
            .into_iter()
            .map(|file| file.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![String::from("deep.txt"), String::from("report.md")]
        );

        // The file budget is a hard stop, not a suggestion.
        assert_eq!(scan_workspace_root(&root, ASSET_SCAN_MAX_DEPTH, 1).len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_index_dedupes_by_path_and_answers_newest_first() {
        let root = asset_test_dir("index");
        let nested = root.join("nested");
        let mut index = AssetIndex::default();

        let old = root.join("old.txt");
        let new = root.join("new.txt");
        assert!(index.upsert(test_asset_entry(&old, &root, 1_000)));
        assert!(index.upsert(test_asset_entry(&new, &root, 2_000)));
        // The same file seen again by a later scan updates it in place.
        let mut rescanned = test_asset_entry(&old, &root, 3_000);
        rescanned.size = 4_096;
        assert!(!index.upsert(rescanned));
        assert_eq!(index.entries.len(), 2);

        // `test_asset_entry` puts everything in workspace "wA".
        let scope_a = AssetScope::Workspace("wA".into());
        let scope_b = AssetScope::Workspace("wB".into());
        let listed = index.session_assets("default", &scope_a, None, 10);
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>(),
            vec![String::from("old.txt"), String::from("new.txt")]
        );
        assert_eq!(listed[0].size, 4_096);
        assert_eq!(listed[0].modified_unix_ms, 3_000);

        // `since` is exclusive, and `limit` cuts the newest page.
        assert_eq!(
            index
                .session_assets("default", &scope_a, Some(2_000), 10)
                .len(),
            1
        );
        assert_eq!(
            index
                .session_assets("default", &scope_a, Some(3_000), 10)
                .len(),
            0
        );
        assert_eq!(index.session_assets("default", &scope_a, None, 1).len(), 1);
        assert_eq!(index.session_assets("other", &scope_a, None, 10).len(), 0);
        // Same session, a different workspace: none of "wA"'s files leak into
        // it. This is the defect the workspace scope closes -- everything
        // above proves the index still works exactly as it did, this proves
        // it no longer answers wider than the workspace asked for.
        assert_eq!(index.session_assets("default", &scope_b, None, 10).len(), 0);

        // Nested roots see the same file; the deeper one owns it.
        let shared = nested.join("shared.txt");
        assert!(index.upsert(test_asset_entry(&shared, &root, 4_000)));
        let mut deeper = test_asset_entry(&shared, &nested, 4_000);
        deeper.workspace_id = Some("wB".into());
        assert!(!index.upsert(deeper));
        let owned = index.get(&asset_id(&shared)).unwrap();
        assert_eq!(owned.root, nested);
        assert_eq!(owned.workspace_id.as_deref(), Some("wB"));
        // A shallower root does not take it back.
        let mut shallower = test_asset_entry(&shared, &root, 5_000);
        shallower.workspace_id = Some("wA".into());
        assert!(!index.upsert(shallower));
        assert_eq!(index.get(&asset_id(&shared)).unwrap().root, nested);

        // A removed worktree takes its files with it.
        index.forget_under(&nested);
        assert!(index.get(&asset_id(&shared)).is_none());
        assert_eq!(index.entries.len(), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_kind_allow_list_is_normalized_and_an_absent_one_filters_nothing() {
        assert!(asset_kind_filter(None).is_empty());
        assert!(asset_kind_filter(Some("")).is_empty());
        // A trailing comma is a client's join, not a kind.
        assert!(asset_kind_filter(Some(",, ,")).is_empty());
        assert_eq!(
            asset_kind_filter(Some("image")),
            vec![String::from("image")]
        );
        assert_eq!(
            asset_kind_filter(Some(" Markdown , PDF ")),
            vec![String::from("markdown"), String::from("pdf")]
        );
        // A kind outside the taxonomy is carried as asked and simply matches
        // nothing, the way an unknown name in the events allow-list does.
        assert_eq!(
            asset_kind_filter(Some("document")),
            vec![String::from("document")]
        );
    }

    #[tokio::test]
    async fn a_kind_filter_answers_the_newest_of_that_kind_not_the_kind_among_the_newest() {
        // The shape that made the filter necessary: an agent editing source code
        // writes files faster than it writes artifacts, so the image and the
        // documents sit well behind the newest page.
        let root = asset_test_dir("kind-listing");
        std::fs::write(root.join("chart.png"), png_bytes()).unwrap();
        std::fs::write(root.join("notes.md"), b"# notes\n").unwrap();
        std::fs::write(root.join("report.pdf"), b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n").unwrap();
        for index in 0..3 {
            std::fs::write(root.join(format!("mod{index}.rs")), b"fn main() {}\n").unwrap();
        }
        let state = asset_listing_state(
            &root,
            vec![
                test_asset_entry(&root.join("chart.png"), &root, 1_000),
                test_asset_entry(&root.join("notes.md"), &root, 2_000),
                test_asset_entry(&root.join("report.pdf"), &root, 3_000),
                test_asset_entry(&root.join("mod0.rs"), &root, 4_000),
                test_asset_entry(&root.join("mod1.rs"), &root, 5_000),
                test_asset_entry(&root.join("mod2.rs"), &root, 6_000),
            ],
        );

        // No kind is the old answer exactly: the newest files, whatever they are.
        assert_eq!(
            listed_asset_names(&state, None, 3).await,
            vec![
                String::from("mod2.rs"),
                String::from("mod1.rs"),
                String::from("mod0.rs")
            ]
        );
        assert_eq!(listed_asset_names(&state, Some(""), 3).await.len(), 3);

        // The image is the fourth-oldest file of six, so a page of three would
        // never have shown it. Filtering during the scan is what finds it.
        assert_eq!(
            listed_asset_names(&state, Some("image"), 3).await,
            vec![String::from("chart.png")]
        );
        // One request for a client filter that spans two kinds, still newest
        // first across both.
        assert_eq!(
            listed_asset_names(&state, Some("markdown,pdf"), 3).await,
            vec![String::from("report.pdf"), String::from("notes.md")]
        );
        // The page is still cut to the limit, and cut from the matches.
        assert_eq!(
            listed_asset_names(&state, Some("text"), 2).await,
            vec![String::from("mod2.rs"), String::from("mod1.rs")]
        );
        // A kind the gateway does not have matches nothing rather than erroring
        // or quietly widening back to everything.
        assert!(listed_asset_names(&state, Some("document"), 3)
            .await
            .is_empty());

        // The applied allow-list comes back, so a client can tell this gateway
        // from one old enough to have ignored the parameter.
        let response = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(Some("Image"), 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(response.0["data"]["kind"], json!(["image"]));
        assert_eq!(response.0["data"]["assets"][0]["kind"], "image");
        let unfiltered = session_assets(
            State(state.clone()),
            Path(("default".into(), "wA".into())),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(unfiltered.0["data"]["kind"], json!([]));

        // A different workspace in the same session sees none of it: the
        // scope this fix adds is the workspace, not the session, and this is
        // the reported defect in miniature -- another workspace's files never
        // showing up in this one's listing.
        let other_workspace = session_assets(
            State(state.clone()),
            Path(("default".into(), "wB".into())),
            Query(asset_listing_query(None, 3)),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert!(other_workspace.0["data"]["assets"]
            .as_array()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_page_is_filled_from_the_matches_and_sniffs_no_further_than_it_has_to() {
        let root = asset_test_dir("kind-page");
        std::fs::write(root.join("a.png"), png_bytes()).unwrap();
        std::fs::write(root.join("b.png"), png_bytes()).unwrap();
        std::fs::write(root.join("c.txt"), b"plain\n").unwrap();
        let ordered = vec![
            test_asset_entry(&root.join("a.png"), &root, 3_000),
            test_asset_entry(&root.join("b.png"), &root, 2_000),
            test_asset_entry(&root.join("c.txt"), &root, 1_000),
        ];

        let names = |page: Vec<Value>| -> Vec<String> {
            page.iter()
                .map(|asset| asset["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            names(asset_page(ordered.clone(), &[], 2)),
            vec![String::from("a.png"), String::from("b.png")]
        );
        assert_eq!(
            names(asset_page(ordered.clone(), &[String::from("image")], 1)),
            vec![String::from("a.png")]
        );
        // A name that lies is not what the filter goes on: the kind is the one
        // sniffed from the bytes, the same one the asset carries on the wire.
        std::fs::write(root.join("a.png"), b"not an image at all\n").unwrap();
        assert_eq!(
            names(asset_page(ordered, &[String::from("image")], 2)),
            vec![String::from("b.png")]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_exact_path_lookup_reaches_an_upload_only_through_the_uploads_root() {
        // The path a client is guaranteed to hold for an upload is the one the
        // upload response returned -- the gateway's own directory, never under
        // a pane's cwd. Without the fold-in, that exact path was the one
        // lookup that could never answer.
        let base = asset_test_dir("uploads-by-path");
        let uploads = base.join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        let stored = uploads.join("9126cf50.webp");
        std::fs::write(&stored, b"fake webp bytes").unwrap();
        let pane_roots = vec![AssetRoot {
            path: base.join("workspace"),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        }];

        // Pane roots alone miss it; the composed lookup roots answer it.
        assert!(asset_entry_for_path(&stored.to_string_lossy(), &pane_roots).is_none());
        let composed = with_uploads_root(pane_roots.clone(), "default", Some(uploads.clone()));
        let found = asset_entry_for_path(&stored.to_string_lossy(), &composed).unwrap();
        assert_eq!(found.path, stored);
        assert_eq!(found.session_id, "default");

        // No uploads directory resolved leaves the roots untouched.
        assert_eq!(
            with_uploads_root(pane_roots.clone(), "default", None).len(),
            1
        );

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn explicit_preview_accepts_home_files_without_widening_scan_roots() {
        let base = asset_test_dir("home-preview");
        let home = base.join("home");
        let workspace = home.join("app");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(home.join("docs")).unwrap();
        std::fs::create_dir_all(home.join(".config")).unwrap();
        let report = home.join("docs/report.md");
        let config = home.join(".config/example.txt");
        let outside = base.join("outside.txt");
        std::fs::write(&report, b"report").unwrap();
        std::fs::write(&config, b"configuration").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        let scan_roots = vec![AssetRoot {
            path: workspace,
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: None,
            pane_id: None,
        }];
        let roots = preview_lookup_roots(
            scan_roots.clone(),
            "default",
            Some(&home),
            [base.clone(), home.clone(), PathBuf::from("/")],
        );
        assert_eq!(scan_roots.len(), 1);
        assert!(asset_entry_for_path(&report.to_string_lossy(), &scan_roots).is_none());
        assert_eq!(roots.len(), 2);
        for path in [&report, &config] {
            let entry = asset_entry_for_path(&path.to_string_lossy(), &roots).unwrap();
            assert_eq!(entry.session_id, "default");
            assert_eq!(
                resolve_indexed_asset_path(&entry.path, &[]),
                Some(entry.path)
            );
        }
        assert!(asset_entry_for_path(&outside.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(&home.to_string_lossy(), &roots).is_none());
        #[cfg(unix)]
        {
            let link = home.join("escape.txt");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(asset_entry_for_path(&link.to_string_lossy(), &roots).is_none());
        }
        assert!(preview_lookup_roots(vec![], "default", Some(FsPath::new("/")), []).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_exact_path_lookup_answers_one_asset_or_none_and_never_leaves_the_roots() {
        let base = asset_test_dir("lookup");
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(workspace.join("a/b/c/d/e/f")).unwrap();
        std::fs::create_dir_all(workspace.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let report = workspace.join("report.md");
        std::fs::write(&report, b"# report\n").unwrap();
        // Deeper than the scan goes, and inside a directory the scan skips: the
        // user pointed at these, so an exact lookup still resolves them.
        let deep = workspace.join("a/b/c/d/e/f/deep.txt");
        std::fs::write(&deep, b"deep\n").unwrap();
        let skipped = workspace.join("node_modules/pkg/index.js");
        std::fs::write(&skipped, b"module\n").unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, b"secret\n").unwrap();
        let roots = vec![AssetRoot {
            path: workspace.clone(),
            session_id: "default".into(),
            workspace_id: Some("wA".into()),
            tab_id: Some("wA:t1".into()),
            pane_id: Some("wA:p1".into()),
        }];

        let found = asset_entry_for_path(&report.to_string_lossy(), &roots).unwrap();
        assert_eq!(found.path, report);
        assert_eq!(found.id, asset_id(&report));
        assert_eq!(found.name, "report.md");
        assert_eq!(found.workspace_id.as_deref(), Some("wA"));
        assert!(asset_entry_for_path(&deep.to_string_lossy(), &roots).is_some());
        assert!(asset_entry_for_path(&skipped.to_string_lossy(), &roots).is_some());

        // The fence still holds, and a fenced-out path is simply a miss.
        assert!(asset_entry_for_path(&secret.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(
            &workspace.join("../outside/secret.txt").to_string_lossy(),
            &roots
        )
        .is_none());
        assert!(asset_entry_for_path("/etc/hosts", &roots).is_none());
        assert!(asset_entry_for_path(&workspace.to_string_lossy(), &roots).is_none());
        assert!(asset_entry_for_path(&workspace.join("a").to_string_lossy(), &roots).is_none());
        assert!(
            asset_entry_for_path(&workspace.join("gone.md").to_string_lossy(), &roots).is_none()
        );
        assert!(asset_entry_for_path("report.md", &roots).is_none());
        assert!(asset_entry_for_path(&report.to_string_lossy(), &[]).is_none());

        // Nested roots: the deepest one owns the file it contains.
        let nested = AssetRoot {
            path: workspace.join("a"),
            session_id: "default".into(),
            workspace_id: Some("wB".into()),
            tab_id: Some("wB:t1".into()),
            pane_id: Some("wB:p1".into()),
        };
        let mut both = roots.clone();
        both.push(nested);
        let owned = asset_entry_for_path(&deep.to_string_lossy(), &both).unwrap();
        assert_eq!(owned.workspace_id.as_deref(), Some("wB"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn asset_ids_are_stable_per_path_and_carry_no_path() {
        let first = asset_id(FsPath::new("/tmp/workspace/report.md"));
        assert_eq!(first, asset_id(FsPath::new("/tmp/workspace/report.md")));
        assert_ne!(first, asset_id(FsPath::new("/tmp/workspace/report2.md")));
        assert!(first.starts_with("as_"));
        assert!(!first.contains("report"));
        assert!(first[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn workspace_roots_come_from_pane_cwds_and_never_widen_to_the_whole_machine() {
        let home = dirs::home_dir().unwrap();
        let response = json!({
            "result": { "panes": [
                { "pane_id": "wA:p1", "workspace_id": "wA", "tab_id": "wA:t1", "cwd": "/Users/okk/.repos/muqun" },
                // A second pane in the same directory is the same root.
                { "pane_id": "wA:p2", "workspace_id": "wA", "tab_id": "wA:t1", "cwd": "/Users/okk/.repos/muqun" },
                { "pane_id": "wB:p1", "workspace_id": "wB", "tab_id": "wB:t1", "foreground_cwd": "/Users/okk/.ws/api" },
                { "pane_id": "wC:p1", "workspace_id": "wC", "cwd": "/" },
                { "pane_id": "wD:p1", "workspace_id": "wD", "cwd": home.to_string_lossy() },
                { "pane_id": "wE:p1", "workspace_id": "wE" }
            ] }
        });
        let roots = pane_list_roots("default", &response);
        assert_eq!(
            roots
                .iter()
                .map(|root| root.path.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec![
                String::from("/Users/okk/.repos/muqun"),
                String::from("/Users/okk/.ws/api")
            ]
        );
        assert_eq!(roots[0].pane_id.as_deref(), Some("wA:p1"));
        assert_eq!(roots[0].tab_id.as_deref(), Some("wA:t1"));
        assert_eq!(roots[1].workspace_id.as_deref(), Some("wB"));
        assert_eq!(roots[1].tab_id.as_deref(), Some("wB:t1"));
        assert!(pane_list_roots("default", &json!({ "result": {} })).is_empty());
    }

    #[test]
    fn asset_scope_narrows_to_a_tab_or_to_a_workspace_and_never_to_the_other() {
        // The invariant card #802's tab-scoping stands on: a `Tab` scope only
        // ever matches its own tmux window, and a `Workspace` scope (what a
        // herdr session resolves its tab to) matches every tab inside that
        // workspace -- so a herdr workspace with more than one tab keeps
        // seeing all of them, exactly as it did before tabs existed here.
        let root_in_tab_a = AssetRoot {
            path: PathBuf::from("/work/a"),
            session_id: "default".into(),
            workspace_id: Some("wM".into()),
            tab_id: Some("wM:t1".into()),
            pane_id: Some("wM:t1:p1".into()),
        };
        let root_in_tab_b = AssetRoot {
            path: PathBuf::from("/work/b"),
            session_id: "default".into(),
            workspace_id: Some("wM".into()),
            tab_id: Some("wM:t2".into()),
            pane_id: Some("wM:t2:p1".into()),
        };
        let root_elsewhere = AssetRoot {
            path: PathBuf::from("/work/c"),
            session_id: "default".into(),
            workspace_id: Some("wN".into()),
            tab_id: Some("wN:t1".into()),
            pane_id: Some("wN:t1:p1".into()),
        };

        let tab_scope = AssetScope::Tab("wM:t1".into());
        assert!(tab_scope.matches_root(&root_in_tab_a));
        assert!(!tab_scope.matches_root(&root_in_tab_b));
        assert!(!tab_scope.matches_root(&root_elsewhere));

        // A herdr session's tab id resolves to its workspace before scoping,
        // so both of that workspace's tabs match -- this is the "herdr must
        // not narrow" guarantee, proven at the layer that actually filters.
        let workspace_scope = AssetScope::Workspace("wM".into());
        assert!(workspace_scope.matches_root(&root_in_tab_a));
        assert!(workspace_scope.matches_root(&root_in_tab_b));
        assert!(!workspace_scope.matches_root(&root_elsewhere));
    }

    #[tokio::test]
    async fn a_tmux_session_scopes_by_tab_directly_and_a_herdr_session_resolves_it_to_a_workspace()
    {
        // tmux: the tab id is used verbatim, no lookup involved.
        let mut session = test_config("token").sessions[0].clone();
        session.backend = BackendKind::Tmux;
        assert_eq!(
            resolve_asset_scope(&session, "@3").await,
            AssetScope::Tab("@3".into())
        );

        // herdr: with no live socket to ask (as in every other test in this
        // file), the tab id can't be translated to its owning workspace, so
        // it is kept as-is rather than silently widened to "no scope at all".
        // This is also exactly what makes every pre-existing
        // `session_assets(Path(("default", "wA")))` test call in this file
        // keep behaving as a workspace-scoped call after this change: they
        // never had a live socket either.
        let mut herdr_session = test_config("token").sessions[0].clone();
        herdr_session.backend = BackendKind::Herdr;
        herdr_session.socket_path = "/tmp/herdr-does-not-exist.sock".into();
        assert_eq!(
            resolve_asset_scope(&herdr_session, "wA").await,
            AssetScope::Workspace("wA".into())
        );
    }

    #[test]
    fn worktree_events_name_a_root_to_scan_and_never_a_file() {
        // The payload Herdr actually sends on protocol 17: the worktree's
        // checkout path and its workspace, with no file information at all.
        let created = json!({
            "event": "worktree_created",
            "data": {
                "type": "worktree_created",
                "workspace": { "workspace_id": "wM", "number": 3, "label": "muqun" },
                "worktree": {
                    "path": "/Users/okk/.repos/muqun/.claude/worktrees/agent-a4463",
                    "branch": "wip/live-activity",
                    "is_bare": false,
                    "is_detached": false,
                    "is_prunable": false,
                    "is_linked_worktree": true,
                    "label": "muqun"
                }
            }
        })
        .to_string();
        let root = worktree_event_root("default", &created).unwrap();
        assert_eq!(
            root.path,
            PathBuf::from("/Users/okk/.repos/muqun/.claude/worktrees/agent-a4463")
        );
        assert_eq!(root.workspace_id.as_deref(), Some("wM"));
        assert_eq!(root.session_id, "default");

        let opened = json!({
            "event": "worktree_opened",
            "data": { "type": "worktree_opened", "already_open": false,
                "workspace": { "workspace_id": "wZ" },
                "worktree": { "path": "/Users/okk/.repos/muqun", "label": "muqun" } }
        })
        .to_string();
        assert_eq!(
            worktree_event_root("default", &opened).unwrap().path,
            PathBuf::from("/Users/okk/.repos/muqun")
        );

        let removed = json!({
            "event": "worktree_removed",
            "data": { "type": "worktree_removed", "forced": false, "workspace_id": "wZ",
                "worktree": { "path": "/Users/okk/.repos/muqun/.claude/worktrees/gone" } }
        })
        .to_string();
        assert_eq!(
            worktree_event_removed_root(&removed).unwrap(),
            PathBuf::from("/Users/okk/.repos/muqun/.claude/worktrees/gone")
        );
        assert!(worktree_event_root("default", &removed).is_none());

        // Everything else on the stream leaves the index alone.
        assert!(worktree_event_root("default", r#"{"event":"pane_updated","data":{}}"#).is_none());
        assert!(worktree_event_root("default", "not json").is_none());
        assert!(worktree_event_removed_root(r#"{"event":"pane_updated","data":{}}"#).is_none());
    }

    #[test]
    fn an_asset_envelope_is_versioned_and_declares_capabilities() {
        let root = PathBuf::from("/tmp/workspace");
        let entry = test_asset_entry(&root.join("report.md"), &root, 1_785_100_000_000);
        let envelope: Value = serde_json::from_str(&asset_created_payload(
            &entry,
            sniff_asset_type(b"# report\n", "report.md"),
        ))
        .unwrap();
        assert_eq!(envelope["schema_version"], CONTENT_SCHEMA_VERSION);
        assert_eq!(envelope["capabilities"]["assets"], true);
        let asset = &envelope["data"]["asset"];
        assert_eq!(asset["id"], entry.id);
        assert_eq!(asset["kind"], "markdown");
        assert_eq!(asset["mime"], "text/markdown; charset=utf-8");
        assert_eq!(asset["previewable"], true);
        assert_eq!(asset["origin"]["session_id"], "default");
        assert_eq!(asset["origin"]["workspace_id"], "wA");
        assert_eq!(asset["origin"]["pane_id"], "wA:p1");
        assert_eq!(asset["modified_unix_ms"], 1_785_100_000_000_u64);
    }

    #[test]
    fn an_asset_file_name_cannot_break_out_of_a_response_header() {
        assert_eq!(header_safe_name("report.md"), "report.md");
        assert_eq!(
            header_safe_name("re\"port\r\nX-Evil: 1.md"),
            "reportX-Evil 1.md"
        );
        assert_eq!(header_safe_name("../../etc/passwd"), "....etcpasswd");
        assert_eq!(header_safe_name("图片.png"), ".png");
        assert_eq!(header_safe_name("\u{202e}"), "asset");
    }

    /// A tmux tab id is a window id: it counts up for the life of the server
    /// and is never reused, so browsing assets across tabs used to add one
    /// permanent entry each, holding that tab's whole `Vec` of root paths.
    #[test]
    fn the_remembered_root_scopes_are_bounded_and_evict_the_oldest() {
        let mut index = AssetIndex::default();
        let root = |path: &str| AssetRoot {
            path: PathBuf::from(path),
            session_id: "default".into(),
            workspace_id: None,
            tab_id: None,
            pane_id: None,
        };
        let scope = |tab: usize| AssetScope::Tab(format!("@{tab}"));

        for tab in 0..MAX_REMEMBERED_ROOT_SCOPES * 3 {
            index.remember_roots("default", Some(&scope(tab)), vec![root("/tmp")]);
            assert!(index.roots.len() <= MAX_REMEMBERED_ROOT_SCOPES);
            assert_eq!(index.roots.len(), index.roots_order.len());
        }
        assert_eq!(index.roots.len(), MAX_REMEMBERED_ROOT_SCOPES);
        // The oldest went, the newest stayed.
        assert!(index.known_roots("default", Some(&scope(0))).is_empty());
        assert!(!index
            .known_roots("default", Some(&scope(MAX_REMEMBERED_ROOT_SCOPES * 3 - 1)))
            .is_empty());

        // Rewriting a scope already held replaces it rather than filling a
        // second slot, or the cap would evict live scopes on a busy session.
        let before = index.roots.len();
        for _ in 0..10 {
            index.remember_roots(
                "default",
                Some(&scope(MAX_REMEMBERED_ROOT_SCOPES * 3 - 1)),
                vec![root("/tmp/again")],
            );
        }
        assert_eq!(index.roots.len(), before);
        assert_eq!(index.roots.len(), index.roots_order.len());
    }
}
