//! The composer's core side: file mentions, the catalogue (commands, skills,
//! models, modes), staging attachments through the gateway's own upload
//! rules, delivering a prompt / command / skill, the queue of prompts waiting
//! for a busy agent, and the current branch of a folder.
//!
//! Like `theme/service.rs` it answers with its own update type
//! ([`ComposerUpdate`]) that the drain hands to `ui/composer/state.rs` on the
//! main thread; nothing here touches Blinc.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use muqun_gateway::agents::{AgentSessionId, ModelRef};
use serde_json::Value;

use super::inbox::{Inbox, Update};
use crate::hub::{CatalogView, HubError, PeerMention, SessionHub};

/// The quiet period before a typed `@word` is searched (the App's 200 ms).
pub const FILE_DEBOUNCE: Duration = Duration::from_millis(200);
/// The gateway's own default page.
pub const FILE_LIMIT: usize = 20;
/// An attachment is read whole before it is sniffed and stored; the gateway's
/// ceiling for video, the largest it accepts.
const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;

/// One match of an `@` search: a path relative to the workspace, inserted
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    pub path: String,
    pub name: String,
    pub kind: String,
}

impl FileHit {
    /// From the gateway's `{path, type}` object. A hit without a path is
    /// not a hit.
    pub fn from_value(value: &Value) -> Option<Self> {
        let path = value.get("path")?.as_str().filter(|p| !p.is_empty())?;
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        Some(Self {
            path: path.to_string(),
            name: name.to_string(),
            kind: value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("file")
                .to_string(),
        })
    }
}

/// One file a queued prompt carries (path for a local thumb, when it is an image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedFile {
    /// Host path (`file://` stripped) or the original URI when that is all we have.
    pub path: String,
    pub name: String,
    pub image: bool,
}

/// One queued or steered prompt waiting for the agent (`Session.Inbox.Info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxItem {
    pub id: String,
    pub text: String,
    /// Runs at the next step boundary rather than in line.
    pub steer: bool,
    pub files: Vec<QueuedFile>,
}

impl InboxItem {
    #[allow(dead_code)]
    pub fn attachments(&self) -> usize {
        self.files.len()
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        let id = value
            .get("id")?
            .as_str()
            .filter(|i| !i.is_empty())?
            .to_string();
        let payload = value.get("payload");
        let text = payload
            .and_then(|p| match p {
                Value::String(s) => Some(s.clone()),
                Value::Object(_) => ["text", "prompt", "message", "content"]
                    .iter()
                    .find_map(|k| p.get(*k).and_then(Value::as_str))
                    .map(str::to_string)
                    .or_else(|| {
                        p.get("parts")?
                            .as_array()?
                            .iter()
                            .find_map(|part| part.get("text")?.as_str().map(str::to_string))
                    }),
                _ => None,
            })
            .unwrap_or_default();
        Some(Self {
            id,
            text,
            steer: value.get("delivery").and_then(Value::as_str) != Some("queue"),
            files: queued_files(value),
        })
    }

    pub fn list(values: &[Value]) -> Vec<Self> {
        values.iter().filter_map(Self::from_value).collect()
    }
}

pub(crate) fn image_name(name: &str) -> bool {
    matches!(
        PathBuf::from(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "heif" | "bmp")
    )
}

fn host_path(raw: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(raw) {
        if url.scheme() == "file" {
            if let Ok(path) = url.to_file_path() {
                return path.display().to_string();
            }
        }
    }
    raw.strip_prefix("file://")
        .map(|rest| rest.strip_prefix("localhost").unwrap_or(rest).to_string())
        .unwrap_or_else(|| raw.to_string())
}

fn queued_file(path: String, name: Option<String>, mime: Option<&str>) -> QueuedFile {
    let name = name.unwrap_or_else(|| {
        PathBuf::from(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into())
    });
    let image = mime
        .map(|m| m.starts_with("image/"))
        .unwrap_or_else(|| image_name(&name) || image_name(&path));
    QueuedFile { path, name, image }
}

fn queued_files(value: &Value) -> Vec<QueuedFile> {
    let payload_files = value
        .get("payload")
        .and_then(|p| p.get("files"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let obj = entry.as_object()?;
                    let source = obj.get("source").and_then(Value::as_object);
                    let raw = ["uri", "url", "path"]
                        .iter()
                        .find_map(|k| obj.get(*k).and_then(Value::as_str))
                        .or_else(|| source?.get("uri")?.as_str())?;
                    let name = obj.get("name").and_then(Value::as_str).map(str::to_string);
                    let mime = obj.get("mime").and_then(Value::as_str);
                    Some(queued_file(host_path(raw), name, mime))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let top = value
        .get("attachments")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, entry)| match entry {
                    Value::String(uri) => {
                        let hint = payload_files.get(i);
                        Some(queued_file(
                            host_path(uri),
                            hint.map(|f| f.name.clone()),
                            None,
                        ))
                    }
                    Value::Object(obj) => {
                        let raw = ["uri", "url", "path"]
                            .iter()
                            .find_map(|k| obj.get(*k).and_then(Value::as_str))?;
                        let name = obj.get("name").and_then(Value::as_str).map(str::to_string);
                        let mime = obj.get("mime").and_then(Value::as_str);
                        Some(queued_file(host_path(raw), name, mime))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !payload_files.is_empty() {
        return payload_files
            .into_iter()
            .enumerate()
            .map(|(i, mut file)| {
                if let Some(att) = top.get(i) {
                    file.image = file.image || att.image;
                    if file.name == "attachment" && att.name != "attachment" {
                        file.name = att.name.clone();
                    }
                }
                file
            })
            .collect();
    }
    top
}

/// Where an attachment's bytes come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A file the owner picked.
    Path(PathBuf),
    /// A PNG from the clipboard.
    Png(Vec<u8>),
}

/// A stored attachment, as the prompt carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    /// Inline image pill linked to this staged upload, when present.
    pub draft_token: Option<u64>,
    /// The host path inside the gateway's upload directory.
    pub path: String,
    pub name: String,
    pub size: usize,
    pub image: bool,
}

/// Where a prompt goes.
#[derive(Debug, Clone)]
pub enum Target {
    Session(AgentSessionId),
    /// A new session in `cwd`, made first (the start page's Start).
    New {
        agent: String,
        cwd: PathBuf,
        model: Option<ModelRef>,
        mode: Option<String>,
        /// Create a sibling git worktree of `cwd` first, then start there.
        worktree: bool,
    },
}

/// What is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deliver {
    Prompt(String),
    /// A host command through the agent; never typed to the model.
    Command {
        name: String,
        args: String,
    },
    Skill {
        name: String,
        args: String,
    },
}

#[derive(Debug, Clone)]
pub enum ComposeCommand {
    /// `@word`: files under the folder, after a quiet period; only the
    /// newest generation is answered.
    FindFiles {
        generation: u64,
        agent: String,
        directory: String,
        query: String,
    },
    Catalog {
        request: u64,
        agent: String,
        directory: Option<String>,
        asid: Option<AgentSessionId>,
    },
    Stage {
        #[allow(dead_code)]
        id: u64,
        name: String,
        source: Source,
    },
    Submit {
        token: u64,
        target: Target,
        deliver: Deliver,
        attachments: Vec<String>,
        /// `queue` when the agent is busy, else the agent's default.
        delivery: Option<String>,
        /// Only enabled for explicit Smart Orchestration sessions so ordinary
        /// user sessions are never silently elevated into bypass mode.
        auto_approve: bool,
    },
    SwitchModel {
        asid: AgentSessionId,
        model: ModelRef,
    },
    SwitchMode {
        asid: AgentSessionId,
        mode: String,
    },
    /// Read whether the session runs in the agent's bypass mode (YOLO).
    AutoApprove {
        asid: AgentSessionId,
    },
    /// Switch it; the answer is the state the agent reports afterwards.
    SetAutoApprove {
        asid: AgentSessionId,
        enabled: bool,
    },
    InboxCancel {
        asid: AgentSessionId,
        item: String,
    },
    InboxDelivery {
        asid: AgentSessionId,
        item: String,
        steer: bool,
    },
    /// The checked-out branch and local branch list of a folder.
    Branch {
        cwd: String,
    },
    /// `git checkout` in `cwd`'s repository (start page only).
    Checkout {
        cwd: String,
        branch: String,
    },
    /// Revert changes to a single file (tracked or untracked inside the repo).
    DiscardFile {
        cwd: String,
        path: String,
        asid: Option<AgentSessionId>,
    },
    /// Computers `@` may name (this machine + linked peers when available).
    ListPeers,
}

/// Detected Git hosting provider for a repository's `origin` remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitRemoteHost {
    GitHub,
    GitLab,
    Gitea,
    Bitbucket,
    Remote,
}

impl GitRemoteHost {
    /// Classify a normalized `http(s)://host/path` repository web URL by host.
    pub fn detect(web_url: &str) -> Self {
        let host = web_url
            .trim()
            .strip_prefix("https://")
            .or_else(|| web_url.trim().strip_prefix("http://"))
            .unwrap_or(web_url.trim())
            .split('/')
            .next()
            .unwrap_or("")
            .rsplit('@')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if host == "github.com" || host.ends_with(".github.com") || host.contains("github") {
            Self::GitHub
        } else if host == "gitlab.com" || host.ends_with(".gitlab.com") || host.contains("gitlab") {
            Self::GitLab
        } else if host == "bitbucket.org"
            || host.ends_with(".bitbucket.org")
            || host.contains("bitbucket")
        {
            Self::Bitbucket
        } else if host == "gitea.com"
            || host.contains("gitea")
            || host.contains("forgejo")
            || host == "codeberg.org"
        {
            Self::Gitea
        } else {
            Self::Remote
        }
    }

    pub fn view_repo_label(self) -> &'static str {
        match self {
            Self::GitHub => "View on GitHub",
            Self::GitLab => "View on GitLab",
            Self::Gitea => "View on Gitea",
            Self::Bitbucket => "View on Bitbucket",
            Self::Remote => "Open Remote",
        }
    }

    pub fn pr_action_label(self, is_feature_branch: bool) -> &'static str {
        match self {
            Self::GitLab if is_feature_branch => "Create Merge Request",
            Self::GitLab => "View Merge Requests",
            Self::Gitea => "View Pull Requests",
            _ if is_feature_branch => "Create Pull Request",
            _ => "View Pull Requests",
        }
    }

    /// Build the provider-appropriate Pull/Merge Request URL for `web_url` and `branch`.
    pub fn pr_url(self, web_url: &str, branch: Option<&str>) -> String {
        let base = web_url.trim().trim_end_matches('/');
        let feature = branch
            .map(str::trim)
            .filter(|b| !b.is_empty() && !matches!(*b, "main" | "master" | "detached"));
        match self {
            Self::GitHub => match feature {
                Some(b) => format!("{base}/pull/new/{b}"),
                None => format!("{base}/pulls"),
            },
            Self::GitLab => match feature {
                Some(b) => {
                    format!("{base}/-/merge_requests/new?merge_request%5Bsource_branch%5D={b}")
                }
                None => format!("{base}/-/merge_requests"),
            },
            Self::Gitea => format!("{base}/pulls"),
            Self::Bitbucket => match feature {
                Some(b) => format!("{base}/pull-requests/new?source={b}"),
                None => format!("{base}/pull-requests"),
            },
            Self::Remote => base.to_string(),
        }
    }
}

/// What the branch pill and Git inspector know about one folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchInfo {
    pub current: Option<String>,
    pub local: Vec<String>,
    pub remote_url: Option<String>,
    pub remote_host: Option<GitRemoteHost>,
}

#[derive(Debug, Clone)]
pub enum ComposerUpdate {
    Files {
        hits: Vec<FileHit>,
    },
    SessionCatalogChanged(AgentSessionId),
    Catalog {
        request: u64,
        asid: Option<AgentSessionId>,
        agent: String,
        directory: Option<String>,
        view: Option<Arc<CatalogView>>,
    },
    Staged {
        #[allow(dead_code)]
        id: u64,
        result: Result<StagedFile, String>,
    },
    /// A submit finished: the draft may go only when `ok`.
    Submitted {
        token: u64,
        ok: bool,
    },
    Branch {
        cwd: String,
        info: BranchInfo,
    },
    Peers {
        peers: Vec<PeerMention>,
    },
    /// The bypass state of one session; `None` when it could not be read
    /// (the shown state stays, and a pending switch is over).
    AutoApprove {
        asid: String,
        enabled: Option<bool>,
    },
}

/// The newest `@` search asked for; older ones drop out after their pause.
static LATEST_FILES: AtomicU64 = AtomicU64::new(0);

fn push(inbox: &Inbox, update: ComposerUpdate) {
    inbox.push(Update::Composer(Box::new(update)));
}

/// Deliver `deliver` to a session the way the App does: a prompt with its
/// files and delivery, a host command, or a skill (which takes no
/// arguments -- `args` are said, not sent, by the caller).
pub async fn deliver(
    hub: &dyn SessionHub,
    asid: &AgentSessionId,
    what: &Deliver,
    attachments: &[String],
    delivery: Option<&str>,
) -> Result<(), HubError> {
    match what {
        Deliver::Prompt(text) => hub.send(asid, text, attachments, delivery).await,
        Deliver::Command { name, args } => hub
            .run_command(asid, name, args, delivery)
            .await
            .map(|_| ()),
        // Agent-session skills take no wire arguments (App `invokeAgentSkill`);
        // typed args are said in the UI, not dropped silently.
        Deliver::Skill { name, .. } => hub.activate_skill(asid, name).await,
    }
}

/// Everything but [`ComposeCommand::Submit`] to a new session, which the
/// core handles with its own session bookkeeping. Returns the error to show,
/// if any.
pub async fn run(
    command: ComposeCommand,
    hub: Arc<dyn SessionHub>,
    inbox: Arc<Inbox>,
) -> Option<HubError> {
    match command {
        ComposeCommand::FindFiles {
            generation,
            agent,
            directory,
            query,
        } => {
            LATEST_FILES.fetch_max(generation, Ordering::SeqCst);
            tokio::time::sleep(FILE_DEBOUNCE).await;
            if LATEST_FILES.load(Ordering::SeqCst) != generation {
                return None;
            }
            // A search that fails is silence: the composer is still a
            // composer without this.
            let found = hub
                .find_files(&agent, &directory, &query, FILE_LIMIT)
                .await
                .unwrap_or_default();
            if LATEST_FILES.load(Ordering::SeqCst) != generation {
                return None;
            }
            let hits = found.iter().filter_map(FileHit::from_value).collect();
            push(&inbox, ComposerUpdate::Files { hits });
            None
        }
        ComposeCommand::Catalog {
            request,
            agent,
            directory,
            asid,
        } => {
            let view = match hub
                .catalog(&agent, asid.as_ref(), directory.as_deref())
                .await
            {
                Ok(view) => {
                    tracing::info!(
                        agent,
                        models = view.catalog.models.len(),
                        modes = view.catalog.modes.len(),
                        commands = view.catalog.commands.len(),
                        skills = view.catalog.skills.len(),
                        "catalogue loaded"
                    );
                    Some(Arc::new(view))
                }
                Err(err) => {
                    tracing::debug!(agent = %agent, %err, "catalogue unavailable");
                    None
                }
            };
            push(
                &inbox,
                ComposerUpdate::Catalog {
                    request,
                    asid,
                    agent,
                    directory,
                    view,
                },
            );
            None
        }
        ComposeCommand::Stage { id, name, source } => {
            let result = stage(hub.as_ref(), &name, source).await;
            push(&inbox, ComposerUpdate::Staged { id, result });
            None
        }
        ComposeCommand::SwitchModel { asid, model } => {
            match hub.switch_model(&asid, &model).await {
                Ok(()) => {
                    push(&inbox, ComposerUpdate::SessionCatalogChanged(asid));
                    None
                }
                Err(err) => Some(err),
            }
        }
        ComposeCommand::SwitchMode { asid, mode } => hub.switch_mode(&asid, &mode).await.err(),
        ComposeCommand::AutoApprove { asid } => {
            let enabled = hub.auto_approve(&asid).await.ok();
            push(
                &inbox,
                ComposerUpdate::AutoApprove {
                    asid: asid.0,
                    enabled,
                },
            );
            None
        }
        ComposeCommand::SetAutoApprove { asid, enabled } => {
            let (shown, error) = match hub.set_auto_approve(&asid, enabled).await {
                Ok(now) => (Some(now), None),
                // Show what the agent really has, and the gateway's reason.
                Err(err) => (hub.auto_approve(&asid).await.ok(), Some(err)),
            };
            push(
                &inbox,
                ComposerUpdate::AutoApprove {
                    asid: asid.0,
                    enabled: shown,
                },
            );
            error
        }
        ComposeCommand::InboxCancel { asid, item } => hub.inbox_cancel(&asid, &item).await.err(),
        ComposeCommand::InboxDelivery { asid, item, steer } => hub
            .inbox_delivery(&asid, &item, if steer { "steer" } else { "queue" })
            .await
            .err(),
        ComposeCommand::Branch { cwd } => {
            let path = PathBuf::from(&cwd);
            let info = tokio::task::spawn_blocking(move || branch_info(&path))
                .await
                .unwrap_or_default();
            push(&inbox, ComposerUpdate::Branch { cwd, info });
            None
        }
        ComposeCommand::Checkout { cwd, branch } => {
            let path = PathBuf::from(&cwd);
            let target = branch.clone();
            let result = tokio::task::spawn_blocking(move || checkout_branch(&path, &target)).await;
            match result {
                Ok(Ok(())) => {
                    let path = PathBuf::from(&cwd);
                    let info = tokio::task::spawn_blocking(move || branch_info(&path))
                        .await
                        .unwrap_or_default();
                    push(&inbox, ComposerUpdate::Branch { cwd, info });
                    None
                }
                Ok(Err(why)) => Some(HubError::Agent(
                    muqun_gateway::agents::ports::AgentError::RequestFailed(why),
                )),
                Err(err) => Some(HubError::Agent(
                    muqun_gateway::agents::ports::AgentError::RequestFailed(err.to_string()),
                )),
            }
        }
        ComposeCommand::DiscardFile { cwd, path, .. } => {
            let path_buf = PathBuf::from(&cwd);
            let target = path.clone();
            let result =
                tokio::task::spawn_blocking(move || discard_file(&path_buf, &target)).await;
            match result {
                Ok(Ok(())) => None,
                Ok(Err(why)) => Some(HubError::Agent(
                    muqun_gateway::agents::ports::AgentError::RequestFailed(why),
                )),
                Err(err) => Some(HubError::Agent(
                    muqun_gateway::agents::ports::AgentError::RequestFailed(err.to_string()),
                )),
            }
        }
        ComposeCommand::ListPeers => {
            let peers = hub.list_peers().await.unwrap_or_default();
            push(&inbox, ComposerUpdate::Peers { peers });
            None
        }
        ComposeCommand::Submit { .. } => unreachable!("handled by the core"),
    }
}

/// Read, check and store one attachment; the error is what the chip says.
async fn stage(hub: &dyn SessionHub, name: &str, source: Source) -> Result<StagedFile, String> {
    let bytes = match source {
        Source::Png(bytes) => bytes,
        Source::Path(path) => {
            let meta = tokio::fs::metadata(&path)
                .await
                .map_err(|err| err.to_string())?;
            if !meta.is_file() {
                return Err("not a file".into());
            }
            if meta.len() > MAX_ATTACHMENT_BYTES {
                return Err(too_large(MAX_ATTACHMENT_BYTES as usize / (1024 * 1024)));
            }
            tokio::fs::read(&path)
                .await
                .map_err(|err| err.to_string())?
        }
    };
    // ACP's image budget is ~3.75 MiB of file bytes; camera stills and
    // screen recordings sit above it. Shrink on a worker the way the App
    // does before the bytes ever hit `store_attachment`.
    let name = name.to_string();
    let (name, bytes) = tokio::task::spawn_blocking(move || {
        crate::platform::media::prepare_attachment(&name, bytes)
    })
    .await
    .map_err(|err| err.to_string())?;
    match hub.store_attachment(&name, bytes).await {
        Ok(stored) => Ok(StagedFile {
            draft_token: None,
            path: stored.path.to_string_lossy().into_owned(),
            name: stored.name,
            size: stored.size,
            image: stored.mime.starts_with("image/"),
        }),
        Err(HubError::Upload(muqun_gateway::platform::UploadRefusal::TooLarge { max_mib })) => {
            Err(too_large(max_mib))
        }
        Err(err) => Err(err.to_string()),
    }
}

fn too_large(max_mib: usize) -> String {
    format!("too large ({max_mib} MB max)")
}

/// First few words of `hint` as a path-safe slug (`fix-the-login` …).
pub fn worktree_slug(hint: &str) -> String {
    let raw: String = hint
        .chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug: String = raw
        .split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "session".into()
    } else {
        slug
    }
}

/// Make a linked worktree next to the repo root of `cwd`, named from `hint`.
pub fn ensure_worktree(cwd: &std::path::Path, hint: &str) -> Result<PathBuf, HubError> {
    create_worktree(cwd, &worktree_slug(hint)).map_err(|err| {
        HubError::Agent(muqun_gateway::agents::ports::AgentError::RequestFailed(err))
    })
}

/// Make a linked worktree next to the repo root of `cwd`, named with `slug`
/// (or a short id). Returns the new worktree path.
pub fn create_worktree(cwd: &std::path::Path, slug: &str) -> Result<PathBuf, String> {
    let root = git_root(cwd).ok_or_else(|| "not a git checkout".to_string())?;
    let base = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("worktree");
    let slug = slug.trim_matches('-');
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() % 100_000)
        .unwrap_or(0);
    let name = if slug.is_empty() {
        format!("{base}-wt-{stamp}")
    } else {
        format!("{base}-{slug}")
    };
    let parent = root
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut path = parent.join(&name);
    let mut n = 2u32;
    while path.exists() {
        path = parent.join(format!("{name}-{n}"));
        n += 1;
    }
    let status = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .map_err(|err| format!("git worktree: {err}"))?;
    if !status.success() {
        return Err("git worktree add failed".into());
    }
    Ok(path)
}

fn git_root(cwd: &std::path::Path) -> Option<PathBuf> {
    for dir in cwd.ancestors().take(24) {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

pub(crate) fn git_dir(cwd: &std::path::Path) -> Option<PathBuf> {
    for dir in cwd.ancestors().take(24) {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let target = PathBuf::from(target);
            return Some(if target.is_absolute() {
                target
            } else {
                dir.join(target)
            });
        }
    }
    None
}

pub(crate) fn parse_git_remote_url(git_dir: &std::path::Path) -> Option<String> {
    let config = std::fs::read_to_string(git_dir.join("config")).ok()?;
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.eq_ignore_ascii_case(r#"[remote "origin"]"#);
        } else if in_origin && line.starts_with("url") {
            if let Some((_, val)) = line.split_once('=') {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

pub(crate) fn parse_repo_web_url(remote_url: &str) -> Option<String> {
    let url = remote_url.trim();
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            let path = path
                .trim_start_matches('/')
                .trim_end_matches('/')
                .trim_end_matches(".git");
            if !host.is_empty() && !path.is_empty() {
                return Some(format!("https://{host}/{path}"));
            }
        }
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.rsplit('@').next().unwrap_or(rest);
        if let Some((host_port, path)) = rest.split_once('/') {
            let host = host_port.split(':').next().unwrap_or(host_port);
            let path = path.trim_end_matches('/').trim_end_matches(".git");
            if !host.is_empty() && !path.is_empty() {
                return Some(format!("https://{host}/{path}"));
            }
        }
    }
    if let Some(scheme) = url
        .strip_prefix("https://")
        .map(|rest| ("https://", rest))
        .or_else(|| url.strip_prefix("http://").map(|rest| ("http://", rest)))
    {
        let (prefix, rest) = scheme;
        let rest = if let Some((authority, path)) = rest.split_once('/') {
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{host}/{path}")
        } else {
            rest.rsplit('@').next().unwrap_or(rest).to_string()
        };
        let clean = rest.trim_end_matches('/').trim_end_matches(".git");
        if !clean.is_empty() {
            return Some(format!("{prefix}{clean}"));
        }
    }
    None
}

fn common_git_dir(git_dir: &std::path::Path) -> PathBuf {
    if let Ok(raw) = std::fs::read_to_string(git_dir.join("commondir")) {
        let rel = raw.trim();
        if !rel.is_empty() {
            let candidate = PathBuf::from(rel);
            return if candidate.is_absolute() {
                candidate
            } else {
                git_dir.join(candidate)
            };
        }
    }
    git_dir.to_path_buf()
}

/// The branch checked out in `cwd` (or the repository above it), read from
/// `.git/HEAD` without running git: `ref: refs/heads/<branch>`, or the short
/// hash of a detached head. A linked worktree's `.git` is a file naming its
/// git directory.
pub fn current_branch(cwd: &std::path::Path) -> Option<String> {
    let git_dir = git_dir(cwd)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_string(),
        None => head.chars().take(7).collect(),
    })
}

/// Current branch, local branch names under `refs/heads`, and parsed `origin` remote info.
pub fn branch_info(cwd: &std::path::Path) -> BranchInfo {
    let current = current_branch(cwd);
    let mut local = Vec::new();
    let Some(git_dir) = git_dir(cwd) else {
        return BranchInfo {
            current,
            local,
            remote_url: None,
            remote_host: None,
        };
    };
    let common_dir = common_git_dir(&git_dir);
    let heads = common_dir.join("refs/heads");
    if heads.is_dir() {
        collect_heads(&heads, "", &mut local);
    }
    if let Ok(packed) = std::fs::read_to_string(common_dir.join("packed-refs")) {
        for line in packed.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let Some(name) = line.split_whitespace().nth(1) else {
                continue;
            };
            if let Some(branch) = name.strip_prefix("refs/heads/") {
                if !local.iter().any(|b| b == branch) {
                    local.push(branch.to_string());
                }
            }
        }
    }
    local.sort();
    if let Some(cur) = current.as_ref() {
        if !local.iter().any(|b| b == cur) && !cur.chars().all(|c| c.is_ascii_hexdigit()) {
            local.insert(0, cur.clone());
        }
    }
    let remote_url = parse_git_remote_url(&common_dir)
        .or_else(|| parse_git_remote_url(&git_dir))
        .and_then(|raw| parse_repo_web_url(&raw));
    let remote_host = remote_url.as_deref().map(GitRemoteHost::detect);
    BranchInfo {
        current,
        local,
        remote_url,
        remote_host,
    }
}

fn collect_heads(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        let full = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_heads(&path, &full, out);
        } else if !out.iter().any(|b| b == &full) {
            out.push(full);
        }
    }
}

/// `git checkout <branch>` at the repository root of `cwd`.
pub fn checkout_branch(cwd: &std::path::Path, branch: &str) -> Result<(), String> {
    let root = git_root(cwd).ok_or_else(|| "not a git checkout".to_string())?;
    let branch = branch.trim();
    if branch.is_empty() || branch.contains([' ', '\n', '\r', '\0']) {
        return Err("invalid branch name".into());
    }
    let output = std::process::Command::new("git")
        .args(["checkout", branch])
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|err| format!("git checkout: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&output.stderr);
    let err = err.trim();
    if err.is_empty() {
        Err("git checkout failed".into())
    } else {
        Err(err.lines().next().unwrap_or(err).to_string())
    }
}

/// Validate a relative repository file path: must stay strictly inside `root`
/// and never touch `.git` or traverse via `..`.
fn validated_repo_rel_path(file_path: &str) -> Result<PathBuf, String> {
    use std::path::{Component, Path};
    let file_path = file_path.trim();
    if file_path.is_empty() || file_path.contains('\0') {
        return Err("invalid file path".into());
    }
    if file_path.starts_with('/') || file_path.starts_with('\\') {
        return Err("path must be relative to the repository".into());
    }
    let rel = Path::new(file_path);
    if rel.is_absolute() {
        return Err("path must be relative to the repository".into());
    }
    let mut clean = PathBuf::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                if part.to_string_lossy().eq_ignore_ascii_case(".git") {
                    return Err("refusing to modify .git metadata".into());
                }
                clean.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("path escapes repository root".into());
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err("invalid file path".into());
    }
    Ok(clean)
}

/// Discard working-tree changes for one file at the repository root of `cwd`.
///
/// Tracked files are restored from `HEAD` (or removed if newly staged without a
/// `HEAD` entry). Untracked files inside the repository root are removed
/// cleanly after refusing `..`, absolute paths, and `.git`.
pub fn discard_file(cwd: &std::path::Path, file_path: &str) -> Result<(), String> {
    let root = git_root(cwd).ok_or_else(|| "not a git checkout".to_string())?;
    let rel = validated_repo_rel_path(file_path)?;
    let rel_str = rel.to_string_lossy().into_owned();

    let ls_files = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", &rel_str])
        .current_dir(&root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|err| format!("git ls-files: {err}"))?;

    if ls_files.success() {
        let output = std::process::Command::new("git")
            .args(["checkout", "HEAD", "--", &rel_str])
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| format!("git checkout: {err}"))?;
        if output.status.success() {
            return Ok(());
        }
        // Tracked in index, e.g. newly staged file not present in HEAD: remove from index & tree.
        let in_head = std::process::Command::new("git")
            .args(["cat-file", "-e", &format!("HEAD:{rel_str}")])
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !in_head {
            let rm = std::process::Command::new("git")
                .args(["rm", "-f", "--", &rel_str])
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .map_err(|err| format!("git rm: {err}"))?;
            if rm.status.success() {
                return Ok(());
            }
        }
        // Fallback if HEAD does not exist (e.g. unborn branch before initial commit)
        if let Ok(fallback) = std::process::Command::new("git")
            .args(["checkout", "--", &rel_str])
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            if fallback.status.success() {
                return Ok(());
            }
        }
        let err = String::from_utf8_lossy(&output.stderr);
        let err = err.trim();
        return Err(if err.is_empty() {
            "git checkout failed".into()
        } else {
            err.lines().next().unwrap_or(err).to_string()
        });
    }

    // Untracked file: verify its parent stays inside the canonical repo root before removing.
    let target = root.join(&rel);
    let canon_root = std::fs::canonicalize(&root).map_err(|e| format!("resolve repo root: {e}"))?;
    let parent = target
        .parent()
        .ok_or_else(|| "invalid file path".to_string())?;
    let canon_parent =
        std::fs::canonicalize(parent).map_err(|e| format!("resolve file parent: {e}"))?;
    if !canon_parent.starts_with(&canon_root) {
        return Err("path escapes repository root".into());
    }
    let meta = std::fs::symlink_metadata(&target).map_err(|e| format!("file not found: {e}"))?;
    if meta.is_dir() {
        return Err("refusing to remove directory".into());
    }
    std::fs::remove_file(&target).map_err(|e| format!("remove untracked file: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbox_items_read_the_payload_however_it_is_shaped() {
        let items = InboxItem::list(&[
            json!({"id": "a", "delivery": "queue", "payload": {"text": "first"}}),
            json!({"id": "b", "payload": "second", "attachments": ["x"]}),
            json!({"id": "c", "payload": {"parts": [{"text": "third"}]}}),
            json!({"payload": "no id"}),
        ]);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].text, "first");
        assert!(!items[0].steer, "queue is not steer");
        assert!(items[1].steer && items[1].attachments() == 1);
        assert_eq!(items[1].files[0].path, "x");
        assert_eq!(items[2].text, "third");
    }

    #[test]
    fn inbox_files_keep_image_paths_for_thumbs() {
        let items = InboxItem::list(&[json!({
            "id": "pic",
            "delivery": "queue",
            "payload": {
                "text": "see this",
                "files": [{"uri": "file:///tmp/shot.png", "name": "shot.png", "mime": "image/png"}]
            },
            "attachments": ["file:///tmp/shot.png"]
        })]);
        assert_eq!(items.len(), 1);
        assert!(items[0].files[0].image);
        assert!(items[0].files[0].path.ends_with("shot.png"));
        assert_eq!(items[0].files[0].name, "shot.png");
    }

    #[test]
    fn inbox_files_prefer_host_upload_path_over_rewritten_image_api_url() {
        let items = InboxItem::list(&[json!({
            "id": "pic-rewritten",
            "delivery": "queue",
            "payload": {
                "text": "edited queued prompt",
                "files": [{
                    "uri": "file:///home/ryu/.local/share/muqun-gateway/uploads/1234.png",
                    "name": "1234.png",
                    "mime": "image/png"
                }]
            },
            "attachments": ["/api/agent-images/ai_abc123/original"]
        })]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].files.len(), 1);
        assert_eq!(
            items[0].files[0].path,
            "/home/ryu/.local/share/muqun-gateway/uploads/1234.png"
        );
        assert_eq!(items[0].files[0].name, "1234.png");
        assert!(items[0].files[0].image);
    }

    #[test]
    fn file_hits_need_a_path() {
        assert!(FileHit::from_value(&json!({"type": "file"})).is_none());
        let hit = FileHit::from_value(&json!({"path": "src/lib.rs"})).unwrap();
        assert_eq!((hit.name.as_str(), hit.kind.as_str()), ("lib.rs", "file"));
    }

    #[test]
    fn worktree_slug_keeps_a_few_words() {
        assert_eq!(
            worktree_slug("Fix the login flow now"),
            "fix-the-login-flow"
        );
        assert_eq!(worktree_slug("!!!"), "session");
        assert_eq!(worktree_slug(""), "session");
    }

    #[test]
    fn the_branch_comes_from_head() {
        let dir = std::env::temp_dir().join(format!("muqun-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/refs/heads/feature")).unwrap();
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::write(dir.join(".git/refs/heads/feature/x"), "abc\n").unwrap();
        std::fs::write(dir.join(".git/refs/heads/main"), "def\n").unwrap();
        assert_eq!(
            current_branch(&dir.join("a/b")).as_deref(),
            Some("feature/x")
        );
        let info = branch_info(&dir.join("a/b"));
        assert_eq!(info.current.as_deref(), Some("feature/x"));
        assert!(info.local.iter().any(|b| b == "main"));
        assert!(info.local.iter().any(|b| b == "feature/x"));
        std::fs::write(dir.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(current_branch(&dir).as_deref(), Some("0123456"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(current_branch(std::path::Path::new("/nonexistent/x")), None);
    }

    #[test]
    fn repo_urls_parse_ssh_and_https() {
        assert_eq!(
            parse_repo_web_url("git@github.com:muqun/desktop.git").as_deref(),
            Some("https://github.com/muqun/desktop")
        );
        assert_eq!(
            parse_repo_web_url("https://github.com/muqun/desktop.git").as_deref(),
            Some("https://github.com/muqun/desktop")
        );
        assert_eq!(
            parse_repo_web_url("https://gitlab.com/group/project").as_deref(),
            Some("https://gitlab.com/group/project")
        );
        assert_eq!(
            parse_repo_web_url("ssh://git@gitea.home.t-diy.com:2222/team/repo.git").as_deref(),
            Some("https://gitea.home.t-diy.com/team/repo")
        );
    }

    #[test]
    fn branch_info_detects_remote_providers_and_pr_urls() {
        let cases = [
            (
                "git@github.com:muqun/desktop.git",
                "https://github.com/muqun/desktop",
                GitRemoteHost::GitHub,
                "https://github.com/muqun/desktop/pull/new/feat/ui",
                "https://github.com/muqun/desktop/pulls",
            ),
            (
                "https://gitlab.com/group/sub/project.git",
                "https://gitlab.com/group/sub/project",
                GitRemoteHost::GitLab,
                "https://gitlab.com/group/sub/project/-/merge_requests/new?merge_request%5Bsource_branch%5D=feat/ui",
                "https://gitlab.com/group/sub/project/-/merge_requests",
            ),
            (
                "ssh://git@gitea.home.t-diy.com:2222/ryu/gateway.git",
                "https://gitea.home.t-diy.com/ryu/gateway",
                GitRemoteHost::Gitea,
                "https://gitea.home.t-diy.com/ryu/gateway/pulls",
                "https://gitea.home.t-diy.com/ryu/gateway/pulls",
            ),
            (
                "git@bitbucket.org:org/app.git",
                "https://bitbucket.org/org/app",
                GitRemoteHost::Bitbucket,
                "https://bitbucket.org/org/app/pull-requests/new?source=feat/ui",
                "https://bitbucket.org/org/app/pull-requests",
            ),
            (
                "https://git.example.org/org/repo.git",
                "https://git.example.org/org/repo",
                GitRemoteHost::Remote,
                "https://git.example.org/org/repo",
                "https://git.example.org/org/repo",
            ),
        ];

        for (i, (raw_remote, expected_web, expected_host, expected_feat_pr, expected_main_pr)) in
            cases.iter().enumerate()
        {
            let dir = std::env::temp_dir().join(format!(
                "muqun-branch-remote-{}-{}",
                std::process::id(),
                i
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
            std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feat/ui\n").unwrap();
            std::fs::write(dir.join(".git/refs/heads/main"), "abc\n").unwrap();
            std::fs::write(
                dir.join(".git/config"),
                format!("[remote \"origin\"]\n\turl = {raw_remote}\n"),
            )
            .unwrap();

            let info = branch_info(&dir);
            assert_eq!(info.remote_url.as_deref(), Some(*expected_web));
            assert_eq!(info.remote_host, Some(*expected_host));
            let host = info.remote_host.unwrap();
            let web = info.remote_url.as_deref().unwrap();
            assert_eq!(host.pr_url(web, Some("feat/ui")), *expected_feat_pr);
            assert_eq!(host.pr_url(web, Some("main")), *expected_main_pr);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn discard_file_handles_tracked_untracked_and_rejects_traversal() {
        assert!(discard_file(std::path::Path::new("/nonexistent"), "test.txt").is_err());

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("muqun-discard-{}-{stamp}", std::process::id()));
        let outside = dir
            .parent()
            .unwrap()
            .join(format!("muqun-outside-{stamp}.txt"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&outside, "keep me\n").unwrap();

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);

        std::fs::write(dir.join("tracked.txt"), "original\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "initial"]);

        // 1. Tracked modified file reverts cleanly to HEAD.
        std::fs::write(dir.join("tracked.txt"), "dirty\n").unwrap();
        discard_file(&dir, "tracked.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
            "original\n"
        );

        // 2. Untracked file inside repo is removed cleanly.
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/untracked.txt"), "scratch\n").unwrap();
        discard_file(&dir, "nested/untracked.txt").unwrap();
        assert!(!dir.join("nested/untracked.txt").exists());

        // 3. Invalid paths, path traversal, absolute paths, and `.git` metadata are rejected.
        assert!(discard_file(&dir, "").is_err());
        assert!(discard_file(&dir, "   ").is_err());
        assert!(discard_file(&dir, "../muqun-outside.txt").is_err());
        assert!(discard_file(
            &dir,
            &format!("../{}", outside.file_name().unwrap().to_string_lossy())
        )
        .is_err());
        assert!(discard_file(&dir, outside.to_str().unwrap()).is_err());
        assert!(discard_file(&dir, ".git/config").is_err());
        assert!(outside.exists(), "outside file must never be deleted");

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
