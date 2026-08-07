//! Starting a new piece of work from the phone.
//!
//! The phone has always been able to talk to an agent that a developer already
//! started at their desk. This module is the other half: handing the phone the
//! one thing it could not do, which is to *begin* a task -- pick a repo, name a
//! branch, choose an agent, and type the first prompt, from the sofa.
//!
//! # What Herdr already does, and what is left here
//!
//! Herdr's socket API (protocol 17) covers almost all of it:
//!
//! * `worktree.list` / `worktree.create` / `worktree.open` / `worktree.remove`
//!   run `git worktree` against a repo Herdr already knows, and -- importantly --
//!   `worktree.create` returns the new workspace, tab and root pane in the same
//!   response. Creating a checkout and getting somewhere to type in it is one
//!   call, not three, so there is no window in which a checkout exists with no
//!   workspace attached.
//! * `workspace.create` does the same minus the checkout, for "just work in the
//!   repo I already have".
//! * `agent.start` launches a supported agent in an existing pane *and waits for
//!   it to become interactive*. That readiness signal is the reason this module
//!   does not sleep-and-hope before typing the first prompt.
//!
//! What is genuinely left to the gateway: deciding whether a repo path is one
//! this session is allowed to touch, deciding whether a branch name is a branch
//! name and not an argument to `git`, and cleaning up after itself when a step
//! in the middle fails.
//!
//! # Why there is still a `git` fallback in here
//!
//! `worktree.create` is the path that runs. The direct-`git` helpers exist for
//! a Herdr that predates those methods, which answers `invalid_request` with
//! "unknown variant" rather than doing the work. They are also the only part of
//! the flow that can be tested for real without a running Herdr, which is why
//! the rollback tests drive them against a temporary repo.
//!
//! # Configuring agents
//!
//! `kind` is Herdr's own agent kind, and Herdr resolves it to the canonical
//! executable. The table below is therefore *not* how the agent is launched --
//! it is how `GET /api/agents/catalog` decides whether a kind is worth offering
//! in a picker on the phone, by looking for the executable on `PATH`.
//! `agent_commands` in `config.json` remaps a kind to a differently named
//! executable, for someone whose `claude` is really `claude-canary`:
//!
//! ```json
//! { "agent_commands": { "claude": "claude-canary" } }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::i18n::{self, Locale};

/// Herdr's supported agent kinds, in the order `herdr agent start --kind`
/// lists them. A kind is also its canonical executable name, which is what
/// makes a `PATH` probe a meaningful availability check.
///
/// A kind missing from here is not rejected outright -- Herdr is the authority
/// on what it can start, and this list only drives the catalogue -- but it will
/// not be offered to the phone.
pub const AGENT_KINDS: &[&str] = &[
    "agy",
    "amp",
    "claude",
    "cline",
    "codex",
    "copilot",
    "cursor",
    "devin",
    "droid",
    "gemini",
    "grok",
    "hermes",
    "kilo",
    "kimi",
    "kiro",
    "maki",
    "mastracode",
    "omp",
    "opencode",
    "pi",
    "qodercli",
];

/// Longest branch name accepted. Git itself is bounded by the filesystem;
/// this is small enough that a ref never becomes a way to push a huge string
/// through the gateway and into a command line.
pub const MAX_BRANCH_NAME_CHARS: usize = 200;

/// Herdr's own default for `agent.start`, restated so the gateway's OpenAPI can
/// document a number rather than "whatever Herdr does".
pub const DEFAULT_AGENT_START_TIMEOUT_MS: u64 = 30_000;
/// Herdr rejects anything outside 3000..=300000, so reject it here with a
/// message that names the field rather than passing it through.
pub const MIN_AGENT_START_TIMEOUT_MS: u64 = 3_001;
pub const MAX_AGENT_START_TIMEOUT_MS: u64 = 300_000;

/// One entry in `GET /api/agents/catalog`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCatalogEntry {
    /// The value to send back as `agent` when creating a task.
    pub kind: String,
    /// Executable looked for on `PATH`. Equal to `kind` unless remapped.
    pub command: String,
    /// Whether that executable was found. False is a hint for the picker, not
    /// a veto: Herdr may still resolve the kind by other means.
    pub available: bool,
    /// Absolute path the probe found, so a developer can see *which* binary
    /// the gateway would be reporting on.
    pub path: Option<String>,
    /// "builtin" or "config", so an override is visible rather than mysterious.
    pub source: &'static str,
}

/// Build the catalogue: every known kind, plus any kind only named in config,
/// each probed on `PATH`.
pub fn agent_catalog(overrides: &BTreeMap<String, String>) -> Vec<AgentCatalogEntry> {
    let mut kinds: Vec<&str> = AGENT_KINDS.to_vec();
    for kind in overrides.keys() {
        if !kinds.contains(&kind.as_str()) {
            kinds.push(kind);
        }
    }
    kinds.sort_unstable();
    kinds
        .into_iter()
        .map(|kind| {
            let command = agent_command(kind, overrides);
            let source = if overrides.contains_key(kind) {
                "config"
            } else {
                "builtin"
            };
            let path = find_on_path(&command);
            AgentCatalogEntry {
                kind: kind.to_owned(),
                available: path.is_some(),
                path: path.map(|path| path.to_string_lossy().into_owned()),
                command,
                source,
            }
        })
        .collect()
}

/// The executable a kind maps to, whether or not it exists.
pub fn agent_command(kind: &str, overrides: &BTreeMap<String, String>) -> String {
    overrides
        .get(kind)
        .cloned()
        .unwrap_or_else(|| kind.to_owned())
}

/// Whether a kind is one the gateway will offer and accept.
pub fn is_known_agent_kind(kind: &str, overrides: &BTreeMap<String, String>) -> bool {
    AGENT_KINDS.contains(&kind) || overrides.contains_key(kind)
}

/// Look one executable up on `PATH`, the way a shell would, minus builtins and
/// aliases. A name carrying a separator is not a `PATH` lookup at all and is
/// refused rather than resolved relative to the gateway's own directory.
pub fn find_on_path(command: &str) -> Option<PathBuf> {
    if command.is_empty() || command.contains('/') || command.contains('\\') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Why a branch name was refused. Returned as the API error message, so each
/// one has to read as an instruction rather than as a regex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchNameError {
    Empty,
    TooLong,
    IllegalCharacter,
    Traversal,
    LeadingDash,
    BadComponent,
    LockSuffix,
}

impl BranchNameError {
    /// The English sentence this refusal is, which is also the key its
    /// translations are filed under.
    ///
    /// It is kept separate from [`BranchNameError::message`] so the catalog has
    /// one obvious place to look these up from, and so a locale with no entry
    /// for a rule still says the rule rather than saying nothing.
    fn english(self) -> &'static str {
        match self {
            Self::Empty => "branch_name must not be empty",
            Self::TooLong => "branch_name must be at most 200 characters",
            Self::IllegalCharacter => {
                "branch_name may only contain letters, digits, dot, underscore, dash and slash"
            }
            Self::Traversal => "branch_name must not contain ..",
            Self::LeadingDash => "branch_name must not start with a dash",
            Self::BadComponent => {
                "branch_name must not have an empty path segment or a segment starting or ending with a dot"
            }
            Self::LockSuffix => "branch_name must not end with .lock",
        }
    }

    /// The refusal as the person who typed the branch name reads it.
    ///
    /// `branch_name` itself never moves: it is the request field the client has
    /// to correct, so naming it in Chinese would name a field that does not
    /// exist. Only the sentence around it is translated.
    pub fn message(self, locale: Locale) -> String {
        i18n::t(locale, self.english()).to_owned()
    }
}

/// The gate on the one field that reaches `git` as an identifier.
///
/// The allow-list is the point: everything that makes a shell or `git` treat an
/// argument as something other than a ref -- a space, a quote, a `$`, a `;`, a
/// leading `-` -- is simply not in it. `..` is called out separately because it
/// survives the character class and is both a git range operator and a path
/// escape, and the per-segment rules keep a name from resolving to a hidden
/// directory or to git's own `.lock` files.
pub fn validate_branch_name(name: &str) -> Result<(), BranchNameError> {
    if name.is_empty() {
        return Err(BranchNameError::Empty);
    }
    if name.chars().count() > MAX_BRANCH_NAME_CHARS {
        return Err(BranchNameError::TooLong);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err(BranchNameError::IllegalCharacter);
    }
    if name.contains("..") {
        return Err(BranchNameError::Traversal);
    }
    if name.starts_with('-') {
        return Err(BranchNameError::LeadingDash);
    }
    if name.ends_with(".lock") {
        return Err(BranchNameError::LockSuffix);
    }
    for segment in name.split('/') {
        if segment.is_empty() || segment.starts_with('.') || segment.ends_with('.') {
            return Err(BranchNameError::BadComponent);
        }
        if segment.ends_with(".lock") {
            return Err(BranchNameError::LockSuffix);
        }
    }
    Ok(())
}

/// Resolve a requested repo path against the roots this session actually has.
///
/// Canonicalising first is what closes symlink escapes, exactly as the asset
/// reads do: a link inside a root that points outside it resolves to the
/// outside path and fails the containment test. Unlike an asset read, a repo
/// path is *allowed to be* a root -- the repo the user is looking at is the
/// obvious thing to branch from -- so the test is equal-or-inside.
pub fn resolve_repo_path(raw: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(raw).ok()?;
    if !canonical.is_dir() {
        return None;
    }
    roots
        .iter()
        .any(|root| canonical == *root || canonical.starts_with(root))
        .then_some(canonical)
}

/// A directory is a git checkout when `.git` is there -- a directory in a normal
/// clone, a file holding a `gitdir:` pointer in a linked worktree. Both count:
/// branching from a worktree is normal.
pub fn is_git_checkout(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Where a new checkout goes when the gateway has to place it itself.
///
/// Beside the repo rather than inside it, so the checkout never lands in the
/// repo's own working tree and shows up as untracked noise. Slashes in the
/// branch flatten to dashes so `feature/login` does not become a nested
/// directory that is awkward to find and awkward to clean up.
pub fn default_worktree_path(repo: &Path, branch: &str) -> Option<PathBuf> {
    let parent = repo.parent()?;
    let repo_name = repo.file_name()?.to_string_lossy().into_owned();
    let slug = branch.replace('/', "-");
    Some(parent.join(format!("{repo_name}-{slug}")))
}

/// Result of the direct-`git` fallback, kept separate from the Herdr path so
/// the caller knows whether *it* is the one that has to clean up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktree {
    pub path: PathBuf,
    /// False when the checkout was already there, which is what makes a retry
    /// from a phone that lost its connection converge instead of erroring.
    pub created: bool,
}

fn git(repo: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|err| anyhow::anyhow!("failed to run git: {err}"))?;
    Ok(output)
}

fn git_checked(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = git(repo, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        anyhow::bail!("git {} failed: {stderr}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether a local branch of that name already exists.
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

/// Path of an existing checkout of `branch`, if this repo has one. Consulted
/// before creating anything, so asking twice for the same branch is a reuse
/// rather than a `git` error about a branch already checked out elsewhere.
pub fn existing_worktree_for_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let listing = git_checked(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut current: Option<PathBuf> = None;
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let name = rest.strip_prefix("refs/heads/").unwrap_or(rest);
            if name == branch {
                return current.clone();
            }
        }
    }
    None
}

/// `git worktree add`, as the gateway would run it if Herdr could not.
///
/// The branch name has already been through [`validate_branch_name`]; `--` is
/// still passed so a name that somehow reached here cannot be read as an option.
pub fn git_worktree_add(repo: &Path, path: &Path, branch: &str) -> anyhow::Result<GitWorktree> {
    if let Some(existing) = existing_worktree_for_branch(repo, branch) {
        return Ok(GitWorktree {
            path: existing,
            created: false,
        });
    }
    if path.exists() {
        anyhow::bail!("{} already exists", path.display());
    }
    let path_arg = path.to_string_lossy().into_owned();
    if branch_exists(repo, branch) {
        git_checked(repo, &["worktree", "add", "--", &path_arg, branch])?;
    } else {
        git_checked(repo, &["worktree", "add", "-b", branch, "--", &path_arg])?;
    }
    Ok(GitWorktree {
        path: path.to_owned(),
        created: true,
    })
}

/// Undo a [`git_worktree_add`] this request made.
///
/// Only ever called for a checkout this request created -- a reused one is left
/// alone, because removing someone else's checkout to tidy up after our own
/// failure would be worse than the failure. The branch is deliberately not
/// deleted: `git worktree remove` does not delete branches, and neither does
/// this. If git refuses (a stray file in the checkout, say) the directory is
/// removed directly and the administrative entry pruned, so a retry is not
/// blocked by our own leftovers.
pub fn git_worktree_remove(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let path_arg = path.to_string_lossy().into_owned();
    let removed = git(repo, &["worktree", "remove", "--force", "--", &path_arg])
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !removed && path.exists() {
        std::fs::remove_dir_all(path)
            .map_err(|err| anyhow::anyhow!("failed to remove {}: {err}", path.display()))?;
    }
    let _ = git(repo, &["worktree", "prune"]);
    if path.exists() {
        anyhow::bail!("{} could not be removed", path.display());
    }
    Ok(())
}

/// The running account of a task dispatch.
///
/// A phone on a train gets a half-finished answer often enough that "it failed"
/// is not a useful response: the user needs to know whether the checkout is
/// there, whether the agent came up, and whether the prompt landed, so they can
/// pick up where it stopped instead of starting again and creating a second
/// copy of everything.
#[derive(Debug, Default, Clone)]
pub struct StepLog {
    steps: Vec<Value>,
}

impl StepLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ok(&mut self, name: &str, detail: Value) {
        self.steps
            .push(json!({ "step": name, "status": "ok", "detail": detail }));
    }

    pub fn skipped(&mut self, name: &str, reason: &str) {
        self.steps
            .push(json!({ "step": name, "status": "skipped", "reason": reason }));
    }

    pub fn failed(&mut self, name: &str, code: &str, message: &str) {
        self.steps.push(json!({
            "step": name,
            "status": "failed",
            "error": { "code": code, "message": message }
        }));
    }

    pub fn rolled_back(&mut self, name: &str, detail: Value) {
        self.steps
            .push(json!({ "step": name, "status": "rolled_back", "detail": detail }));
    }

    pub fn has_failure(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.get("status").and_then(Value::as_str) == Some("failed"))
    }

    pub fn value(&self) -> Value {
        Value::Array(self.steps.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-tasks-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_repo(dir: &Path) {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "--initial-branch", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "init"]] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
    }

    #[test]
    fn a_branch_name_is_a_ref_and_never_an_argument() {
        for good in [
            "feature/login",
            "fix-594",
            "release/2.0",
            "a",
            "user_name/thing.v2",
        ] {
            assert_eq!(validate_branch_name(good), Ok(()), "{good} should be valid");
        }

        // Everything that would turn the value into something other than a ref.
        for (bad, expected) in [
            ("", BranchNameError::Empty),
            ("../../etc/passwd", BranchNameError::Traversal),
            ("feature/../..", BranchNameError::Traversal),
            ("a..b", BranchNameError::Traversal),
            ("--force", BranchNameError::LeadingDash),
            ("-b", BranchNameError::LeadingDash),
            ("feat ure", BranchNameError::IllegalCharacter),
            ("feat;rm -rf /", BranchNameError::IllegalCharacter),
            ("feat$(whoami)", BranchNameError::IllegalCharacter),
            ("feat`id`", BranchNameError::IllegalCharacter),
            ("feat\nrm", BranchNameError::IllegalCharacter),
            ("feat|x", BranchNameError::IllegalCharacter),
            ("feat~1", BranchNameError::IllegalCharacter),
            ("HEAD@{0}", BranchNameError::IllegalCharacter),
            ("feat:x", BranchNameError::IllegalCharacter),
            ("café", BranchNameError::IllegalCharacter),
            ("/leading", BranchNameError::BadComponent),
            ("trailing/", BranchNameError::BadComponent),
            ("double//slash", BranchNameError::BadComponent),
            (".hidden", BranchNameError::BadComponent),
            ("feature/.hidden", BranchNameError::BadComponent),
            ("feature/trailing.", BranchNameError::BadComponent),
            ("feature.lock", BranchNameError::LockSuffix),
        ] {
            assert_eq!(
                validate_branch_name(bad),
                Err(expected),
                "{bad:?} should be rejected"
            );
        }

        assert_eq!(
            validate_branch_name(&"a".repeat(MAX_BRANCH_NAME_CHARS + 1)),
            Err(BranchNameError::TooLong)
        );
    }

    /// Every rule reads as an instruction in either language, and the field it
    /// instructs the client to fix keeps its name.
    #[test]
    fn a_refused_branch_name_explains_itself_in_the_readers_language() {
        for error in [
            BranchNameError::Empty,
            BranchNameError::TooLong,
            BranchNameError::IllegalCharacter,
            BranchNameError::Traversal,
            BranchNameError::LeadingDash,
            BranchNameError::BadComponent,
            BranchNameError::LockSuffix,
        ] {
            let english = error.message(Locale::En);
            let chinese = error.message(Locale::ZhTw);
            assert_eq!(english, error.english(), "English is its own translation");
            assert_ne!(chinese, english, "{english:?} has no translation");
            // `branch_name` is the request field the client has to correct, so
            // it is named identically whichever language the sentence is in.
            assert!(english.starts_with("branch_name "));
            assert!(chinese.starts_with("branch_name "));
        }
        assert_eq!(
            BranchNameError::Empty.message(Locale::ZhTw),
            "branch_name 不得為空"
        );
    }

    #[test]
    fn a_repo_path_has_to_be_one_this_session_already_has() {
        let dir = temp_dir("fence");
        let root = dir.join("workspace");
        let nested = root.join("packages").join("app");
        let outside = dir.join("elsewhere");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();
        let roots = vec![std::fs::canonicalize(&root).unwrap()];

        // The root itself is allowed: branching from the repo you are looking
        // at is the whole point.
        assert!(resolve_repo_path(root.to_str().unwrap(), &roots).is_some());
        assert!(resolve_repo_path(nested.to_str().unwrap(), &roots).is_some());

        // Outside, above, and traversal back out are all refused.
        assert!(resolve_repo_path(outside.to_str().unwrap(), &roots).is_none());
        assert!(resolve_repo_path(dir.to_str().unwrap(), &roots).is_none());
        assert!(resolve_repo_path("/", &roots).is_none());
        assert!(resolve_repo_path("/etc", &roots).is_none());
        assert!(
            resolve_repo_path(root.join("..").join("elsewhere").to_str().unwrap(), &roots)
                .is_none()
        );

        // A file is not a repo, and a missing path is a miss rather than a panic.
        assert!(resolve_repo_path(root.join("file.txt").to_str().unwrap(), &roots).is_none());
        assert!(resolve_repo_path(root.join("nope").to_str().unwrap(), &roots).is_none());

        // A symlink inside a root that points out of it resolves to the outside
        // path, and fails there.
        #[cfg(unix)]
        {
            let escape = root.join("escape");
            std::os::unix::fs::symlink(&outside, &escape).unwrap();
            assert!(resolve_repo_path(escape.to_str().unwrap(), &roots).is_none());
        }

        // With no roots at all nothing is reachable: an empty fence is closed,
        // not open.
        assert!(resolve_repo_path(root.to_str().unwrap(), &[]).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_agent_kind_maps_to_a_command_and_config_can_remap_it() {
        let none = BTreeMap::new();
        assert_eq!(agent_command("claude", &none), "claude");
        assert_eq!(agent_command("codex", &none), "codex");
        assert!(is_known_agent_kind("claude", &none));
        assert!(is_known_agent_kind("codex", &none));
        assert!(!is_known_agent_kind("definitely-not-an-agent", &none));

        let mut overrides = BTreeMap::new();
        overrides.insert("claude".to_owned(), "claude-canary".to_owned());
        overrides.insert("house".to_owned(), "house-agent".to_owned());
        assert_eq!(agent_command("claude", &overrides), "claude-canary");
        assert_eq!(agent_command("codex", &overrides), "codex");
        assert!(is_known_agent_kind("house", &overrides));

        let catalog = agent_catalog(&overrides);
        let claude = catalog.iter().find(|entry| entry.kind == "claude").unwrap();
        assert_eq!(claude.command, "claude-canary");
        assert_eq!(claude.source, "config");
        let codex = catalog.iter().find(|entry| entry.kind == "codex").unwrap();
        assert_eq!(codex.source, "builtin");
        // A config-only kind is offered too, and the built-ins are all still there.
        assert!(catalog.iter().any(|entry| entry.kind == "house"));
        assert_eq!(catalog.len(), AGENT_KINDS.len() + 1);
        // Sorted, so a picker on the phone does not reorder between calls.
        let kinds: Vec<&str> = catalog.iter().map(|entry| entry.kind.as_str()).collect();
        let mut sorted = kinds.clone();
        sorted.sort_unstable();
        assert_eq!(kinds, sorted);
    }

    #[test]
    fn a_path_probe_only_ever_looks_on_path() {
        // A name with a separator is not a PATH lookup, and must not be
        // resolved against the gateway's own working directory.
        assert!(find_on_path("./claude").is_none());
        assert!(find_on_path("/usr/bin/env").is_none());
        assert!(find_on_path("").is_none());
        // Something every POSIX box has, to prove the probe does find things.
        assert!(find_on_path("sh").is_some());
        assert!(find_on_path("muqun-gateway-definitely-not-installed").is_none());
    }

    #[test]
    fn a_new_checkout_lands_beside_the_repo_and_never_inside_it() {
        let repo = PathBuf::from("/Users/dev/code/muqun");
        let path = default_worktree_path(&repo, "feature/login").unwrap();
        assert_eq!(path, PathBuf::from("/Users/dev/code/muqun-feature-login"));
        assert!(!path.starts_with(&repo));
        assert_eq!(
            default_worktree_path(&repo, "fix-594").unwrap(),
            PathBuf::from("/Users/dev/code/muqun-fix-594")
        );
    }

    #[test]
    fn a_worktree_this_request_made_is_removed_again_when_a_later_step_fails() {
        let dir = temp_dir("rollback");
        init_repo(&dir);
        let repo = dir.join("repo");
        let path = default_worktree_path(&repo, "task/594").unwrap();

        let created = git_worktree_add(&repo, &path, "task/594").unwrap();
        assert!(created.created);
        assert!(created.path.is_dir(), "the checkout should exist");
        assert!(created.path.join("README.md").is_file());
        assert!(branch_exists(&repo, "task/594"));

        // The failure happens here. Rollback leaves no checkout and no
        // administrative entry, so the same request can simply be retried.
        git_worktree_remove(&repo, &created.path).unwrap();
        assert!(!created.path.exists(), "the checkout should be gone");
        assert!(
            existing_worktree_for_branch(&repo, "task/594").is_none(),
            "git should not still list the removed checkout"
        );
        // The branch survives: removing a checkout is not deleting work.
        assert!(branch_exists(&repo, "task/594"));

        // And the retry works, which is the point of pruning.
        let again = git_worktree_add(&repo, &path, "task/594").unwrap();
        assert!(again.created);
        assert!(again.path.is_dir());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn asking_twice_for_the_same_branch_reuses_the_checkout_instead_of_making_a_second_one() {
        let dir = temp_dir("idempotent");
        init_repo(&dir);
        let repo = dir.join("repo");
        let path = default_worktree_path(&repo, "task/dup").unwrap();

        let first = git_worktree_add(&repo, &path, "task/dup").unwrap();
        assert!(first.created);

        // A phone that lost its answer and retried must not end up with two
        // checkouts, and must not get an error either.
        let second = git_worktree_add(&repo, &path, "task/dup").unwrap();
        assert!(!second.created, "a retry must not claim to have created it");
        assert_eq!(
            std::fs::canonicalize(&second.path).unwrap(),
            std::fs::canonicalize(&first.path).unwrap()
        );

        // Reuse is only ever reported for a checkout that is really there, so
        // the caller knows not to roll it back.
        assert!(second.path.is_dir());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rollback_that_git_refuses_still_leaves_nothing_behind() {
        let dir = temp_dir("stubborn");
        init_repo(&dir);
        let repo = dir.join("repo");
        let path = default_worktree_path(&repo, "task/dirty").unwrap();
        let created = git_worktree_add(&repo, &path, "task/dirty").unwrap();

        // Untracked work in the checkout is what makes `git worktree remove`
        // baulk without --force, and a stale lock is what makes it baulk even
        // with it. Either way the gateway must not leave the directory behind.
        std::fs::write(created.path.join("scratch.txt"), "unsaved").unwrap();
        git_worktree_remove(&repo, &created.path).unwrap();
        assert!(!created.path.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_recreated() {
        let dir = temp_dir("existing-branch");
        init_repo(&dir);
        let repo = dir.join("repo");
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "already-here"])
            .output()
            .unwrap();
        assert!(output.status.success());

        let path = default_worktree_path(&repo, "already-here").unwrap();
        let created = git_worktree_add(&repo, &path, "already-here").unwrap();
        assert!(created.created);
        let head = git_checked(&created.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, "already-here");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn steps_record_what_happened_so_a_partial_run_can_be_picked_up() {
        let mut log = StepLog::new();
        assert!(!log.has_failure());

        log.ok("worktree", json!({ "path": "/tmp/x", "reused": false }));
        assert!(!log.has_failure(), "an ok step is not a failure");
        log.skipped("prompt", "no prompt was given");
        log.failed("agent", "agent_start_failed", "claude did not become ready");
        assert!(log.has_failure());
        log.rolled_back("worktree", json!({ "path": "/tmp/x" }));

        let value = log.value();
        let steps = value.as_array().unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0]["step"], "worktree");
        assert_eq!(steps[0]["status"], "ok");
        assert_eq!(steps[1]["status"], "skipped");
        assert_eq!(steps[2]["error"]["code"], "agent_start_failed");
        assert_eq!(steps[3]["status"], "rolled_back");
    }

    #[test]
    fn is_git_checkout_accepts_a_clone_and_a_linked_worktree() {
        let dir = temp_dir("checkout");
        init_repo(&dir);
        let repo = dir.join("repo");
        assert!(is_git_checkout(&repo), "a clone has a .git directory");
        assert!(!is_git_checkout(&dir), "its parent does not");

        let path = default_worktree_path(&repo, "linked").unwrap();
        let created = git_worktree_add(&repo, &path, "linked").unwrap();
        assert!(
            is_git_checkout(&created.path),
            "a linked worktree has a .git file, and branching from it is normal"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
