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

/// One queued or steered prompt waiting for the agent (`Session.Inbox.Info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxItem {
    pub id: String,
    pub text: String,
    /// Runs at the next step boundary rather than in line.
    pub steer: bool,
    pub attachments: usize,
}

impl InboxItem {
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
            attachments: value
                .get("attachments")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        })
    }

    pub fn list(values: &[Value]) -> Vec<Self> {
        values.iter().filter_map(Self::from_value).collect()
    }
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
        agent: String,
        directory: Option<String>,
        asid: Option<AgentSessionId>,
    },
    Stage {
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
    /// Computers `@` may name (this machine + linked peers when available).
    ListPeers,
}

/// What the branch pill knows about one folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchInfo {
    pub current: Option<String>,
    pub local: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ComposerUpdate {
    Files {
        hits: Vec<FileHit>,
    },
    Catalog {
        agent: String,
        directory: Option<String>,
        view: Arc<CatalogView>,
    },
    Staged {
        id: u64,
        result: Result<StagedFile, String>,
    },
    /// A submit finished: the draft may go only when `ok`.
    Submitted {
        token: u64,
        ok: bool,
    },
    Inbox {
        asid: String,
        items: Vec<InboxItem>,
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

/// The waiting prompts of a session, to the composer.
pub fn push_inbox(inbox: &Inbox, asid: &str, items: &[Value]) {
    push(
        inbox,
        ComposerUpdate::Inbox {
            asid: asid.to_string(),
            items: InboxItem::list(items),
        },
    );
}

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
            agent,
            directory,
            asid,
        } => {
            match hub
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
                    push(
                        &inbox,
                        ComposerUpdate::Catalog {
                            agent,
                            directory,
                            view: Arc::new(view),
                        },
                    )
                }
                // Not attached yet, or no answer: nothing is stored, and the
                // composer asks again after a pause. Session header still shows
                // the live model/mode without a catalogue.
                Err(err) => tracing::debug!(agent = %agent, %err, "catalogue unavailable"),
            }
            None
        }
        ComposeCommand::Stage { id, name, source } => {
            let result = stage(hub.as_ref(), &name, source).await;
            push(&inbox, ComposerUpdate::Staged { id, result });
            None
        }
        ComposeCommand::SwitchModel { asid, model } => hub.switch_model(&asid, &model).await.err(),
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
    match hub.store_attachment(name, bytes).await {
        Ok(stored) => Ok(StagedFile {
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

fn git_dir(cwd: &std::path::Path) -> Option<PathBuf> {
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

/// Current branch plus local branch names under `refs/heads`.
pub fn branch_info(cwd: &std::path::Path) -> BranchInfo {
    let current = current_branch(cwd);
    let mut local = Vec::new();
    let Some(git_dir) = git_dir(cwd) else {
        return BranchInfo { current, local };
    };
    let heads = git_dir.join("refs/heads");
    if heads.is_dir() {
        collect_heads(&heads, "", &mut local);
    }
    if let Ok(packed) = std::fs::read_to_string(git_dir.join("packed-refs")) {
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
    BranchInfo { current, local }
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
        assert!(items[1].steer && items[1].attachments == 1);
        assert_eq!(items[2].text, "third");
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
}
