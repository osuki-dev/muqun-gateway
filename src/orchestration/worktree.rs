//! Minimal git worktree isolation for parallel MAO subtasks.
//!
//! Creates isolated external worktrees (`<state_dir>/mao-worktrees/...`) so
//! concurrent write agents never stomp on the same working directory. Merging
//! branches back into the target workspace, conflict resolution, and final
//! verification are performed by an Agent (never hardcoded `git apply`).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoContext {
    /// Canonical root of the user's current checkout (regular repo or linked worktree).
    pub toplevel: PathBuf,
    /// Shared `.git` directory (`git rev-parse --git-common-dir`).
    pub common_dir: PathBuf,
    /// True when the user's workspace is itself a linked git worktree.
    pub is_linked_worktree: bool,
    /// Current branch name if HEAD is attached.
    pub branch: Option<String>,
    /// Current HEAD commit SHA.
    pub head_sha: String,
    /// Snapshot commit SHA used as base for child worktrees (`head_sha` when clean,
    /// or a non-destructive `git stash create` commit when dirty).
    pub base_sha: String,
    pub was_dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub branch: String,
    pub base_sha: String,
}

fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_cmd(dir, args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_checked(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git_cmd(dir, args)
        .output()
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "git {} failed: {} {}",
            args.join(" "),
            stderr.trim(),
            stdout.trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_checked_with_identity(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = git_cmd(dir, args);
    if git_stdout(dir, &["config", "user.name"]).is_none() {
        cmd.env("GIT_AUTHOR_NAME", "Muqun Orchestrator")
            .env("GIT_COMMITTER_NAME", "Muqun Orchestrator");
    }
    if git_stdout(dir, &["config", "user.email"]).is_none() {
        cmd.env("GIT_AUTHOR_EMAIL", "mao@muqun.local")
            .env("GIT_COMMITTER_EMAIL", "mao@muqun.local");
    }
    let out = cmd
        .output()
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "git {} failed: {} {}",
            args.join(" "),
            stderr.trim(),
            stdout.trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn sanitize_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(32));
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 32 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "step".into()
    } else {
        trimmed
    }
}

fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Inspect a directory to determine its git checkout context (works inside both
/// a normal git repo and an existing linked git worktree).
pub fn inspect_repo(cwd: &Path) -> Option<RepoContext> {
    if !cwd.is_dir() {
        return None;
    }
    let top_raw = git_stdout(cwd, &["rev-parse", "--show-toplevel"])?;
    let toplevel = PathBuf::from(top_raw.trim());
    if !toplevel.is_dir() {
        return None;
    }
    let toplevel = toplevel.canonicalize().unwrap_or(toplevel);

    let common_raw = git_stdout(&toplevel, &["rev-parse", "--git-common-dir"])?;
    let common_path = PathBuf::from(common_raw.trim());
    let common_dir = if common_path.is_absolute() {
        common_path
    } else {
        toplevel.join(common_path)
    };
    let common_dir = common_dir.canonicalize().unwrap_or(common_dir);

    let is_linked_worktree = toplevel.join(".git").is_file();
    let head_sha = git_stdout(&toplevel, &["rev-parse", "--verify", "HEAD"])?
        .trim()
        .to_string();
    if head_sha.is_empty() {
        return None;
    }
    let branch = git_stdout(&toplevel, &["symbolic-ref", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let status = git_stdout(&toplevel, &["status", "--porcelain"]).unwrap_or_default();
    let was_dirty = !status.trim().is_empty();
    let base_sha = if was_dirty {
        git_stdout(&toplevel, &["stash", "create"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| head_sha.clone())
    } else {
        head_sha.clone()
    };

    Some(RepoContext {
        toplevel,
        common_dir,
        is_linked_worktree,
        branch,
        head_sha,
        base_sha,
        was_dirty,
    })
}

pub fn run_worktrees_root(state_dir: &Path, repo: &RepoContext, run_id: &str) -> PathBuf {
    let repo_key = short_hash(&repo.common_dir.to_string_lossy());
    let run_slug = sanitize_slug(run_id);
    state_dir
        .join("mao-worktrees")
        .join(repo_key)
        .join(run_slug)
}

/// Create an external isolated worktree for one parallel subtask.
pub fn create_step_worktree(
    state_dir: &Path,
    repo: &RepoContext,
    run_id: &str,
    step_id: &str,
) -> Result<WorktreeHandle> {
    let root = run_worktrees_root(state_dir, repo, run_id);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create worktree parent {}", root.display()))?;

    let step_slug = sanitize_slug(step_id);
    let short_run = short_hash(run_id);
    let branch = format!("mao/{short_run}/{step_slug}");
    let path = root.join(&step_slug);

    if path.exists() {
        let _ = git_cmd(
            &repo.toplevel,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        )
        .output();
        let _ = std::fs::remove_dir_all(&path);
    }
    let _ = git_cmd(&repo.toplevel, &["branch", "-D", &branch]).output();

    git_checked(
        &repo.toplevel,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &path.to_string_lossy(),
            &repo.base_sha,
        ],
    )?;

    Ok(WorktreeHandle {
        path,
        branch,
        base_sha: repo.base_sha.clone(),
    })
}

/// Stage and commit any edits a subtask left in its worktree so the Integrator
/// Agent can cleanly merge/cherry-pick the branch.
pub fn commit_worktree_changes(
    wt_path: &Path,
    base_sha: &str,
    message: &str,
) -> Result<Option<String>> {
    if !wt_path.is_dir() {
        return Ok(None);
    }
    let status = git_stdout(wt_path, &["status", "--porcelain"]).unwrap_or_default();
    if !status.trim().is_empty() {
        git_checked(wt_path, &["add", "-A"])?;
        git_checked_with_identity(wt_path, &["commit", "-m", message])?;
    }
    let head = git_checked(wt_path, &["rev-parse", "--verify", "HEAD"])?
        .trim()
        .to_string();
    if head == base_sha {
        Ok(None)
    } else {
        Ok(Some(head))
    }
}

/// Concise diff stat/excerpt of a worktree relative to `base_sha` (used for failover handoff).
pub fn worktree_diff_excerpt(wt_path: &Path, base_sha: &str) -> String {
    if !wt_path.is_dir() || base_sha.trim().is_empty() {
        return String::new();
    }
    let stat = git_stdout(wt_path, &["diff", "--stat", base_sha]).unwrap_or_default();
    let status = git_stdout(wt_path, &["status", "--porcelain"]).unwrap_or_default();
    let mut out = String::new();
    if !status.trim().is_empty() {
        out.push_str(status.trim());
        out.push('\n');
    }
    if !stat.trim().is_empty() {
        out.push_str(stat.trim());
    }
    out
}

/// Best-effort cleanup of managed MAO worktrees and temporary branches.
pub fn cleanup_worktrees(repo_root: &Path, worktree_paths: &[String], branches: &[String]) {
    for path in worktree_paths {
        if path.trim().is_empty() {
            continue;
        }
        let _ = git_cmd(repo_root, &["worktree", "remove", "--force", path]).output();
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = git_cmd(repo_root, &["worktree", "prune"]).output();
    for branch in branches {
        if branch.starts_with("mao/") {
            let _ = git_cmd(repo_root, &["branch", "-D", branch]).output();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_checked(dir, &["init"]).unwrap();
        git_checked(dir, &["config", "user.email", "t@example.com"]).unwrap();
        git_checked(dir, &["config", "user.name", "Tester"]).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        git_checked(dir, &["add", "."]).unwrap();
        git_checked(dir, &["commit", "-m", "init"]).unwrap();
    }

    #[test]
    fn spawns_external_worktrees_from_linked_user_worktree_and_cleans_up() {
        let base = crate::test_state_dir().join(format!("wt-test-{}", uuid::Uuid::new_v4()));
        let main_repo = base.join("main-repo");
        let user_wt = base.join("user-feature-wt");
        let state_dir = base.join("state");
        init_repo(&main_repo);

        git_checked(
            &main_repo,
            &[
                "worktree",
                "add",
                "-b",
                "feat/user-branch",
                &user_wt.to_string_lossy(),
            ],
        )
        .unwrap();

        let ctx = inspect_repo(&user_wt).expect("inspect linked worktree");
        assert!(ctx.is_linked_worktree);
        assert_eq!(ctx.branch.as_deref(), Some("feat/user-branch"));

        let wt1 = create_step_worktree(&state_dir, &ctx, "o_test_1", "backend").unwrap();
        let wt2 = create_step_worktree(&state_dir, &ctx, "o_test_1", "frontend").unwrap();
        assert!(!wt1.path.starts_with(&user_wt));
        assert!(!wt2.path.starts_with(&user_wt));

        std::fs::write(wt1.path.join("a.txt"), "hello backend\n").unwrap();
        let sha = commit_worktree_changes(&wt1.path, &wt1.base_sha, "backend edit").unwrap();
        assert!(sha.is_some());

        cleanup_worktrees(
            &ctx.toplevel,
            &[
                wt1.path.to_string_lossy().into_owned(),
                wt2.path.to_string_lossy().into_owned(),
            ],
            &[wt1.branch, wt2.branch],
        );
        assert!(!wt1.path.exists());
        assert!(!wt2.path.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
