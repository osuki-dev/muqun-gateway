use std::path::{Path, PathBuf};
use std::process::Command;

/// Commit all staged or modified files in `cwd` with `message`.
pub fn git_commit(cwd: &Path, message: &str) -> Result<String, String> {
    let repo = find_repo_root(cwd).ok_or_else(|| "Not a git repository".to_string())?;
    let message = message.trim();
    if message.is_empty() {
        return Err("Commit message cannot be empty".to_string());
    }

    // Stage all changes (like git add -A)
    let add_status = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["add", "-A"])
        .status()
        .map_err(|e| format!("git add failed: {e}"))?;
    if !add_status.success() {
        return Err("Failed to stage changes with git add -A".to_string());
    }

    // Commit
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-m", message])
        .output()
        .map_err(|e| format!("git commit failed: {e}"))?;

    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let summary = stdout
            .lines()
            .next()
            .unwrap_or("Committed changes")
            .trim()
            .to_string();
        Ok(summary)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let err = stderr.trim();
        if err.is_empty() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let out_trim = stdout.trim();
            if out_trim.is_empty() {
                Err("git commit exited with non-zero status".to_string())
            } else {
                Err(out_trim.to_string())
            }
        } else {
            Err(err.to_string())
        }
    }
}

/// Push commits on the current branch to remote upstream.
pub fn git_push(cwd: &Path) -> Result<String, String> {
    let repo = find_repo_root(cwd).ok_or_else(|| "Not a git repository".to_string())?;

    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["push"])
        .output()
        .map_err(|e| format!("git push failed: {e}"))?;

    if out.status.success() {
        let out_str = String::from_utf8_lossy(&out.stdout);
        let err_str = String::from_utf8_lossy(&out.stderr);
        let combined = if !out_str.trim().is_empty() {
            out_str.trim().to_string()
        } else {
            err_str.trim().to_string()
        };
        Ok(combined)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // If current branch has no upstream branch, try git push -u origin <branch>
        if stderr.contains("has no upstream branch") || stderr.contains("set-upstream") {
            if let Some(branch) = crate::model::compose::current_branch(&repo) {
                let retry = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["push", "-u", "origin", &branch])
                    .output()
                    .map_err(|e| format!("git push -u origin failed: {e}"))?;
                if retry.status.success() {
                    return Ok(format!("Pushed and set upstream to origin/{branch}"));
                } else {
                    let err2 = String::from_utf8_lossy(&retry.stderr);
                    return Err(err2.trim().to_string());
                }
            }
        }
        Err(stderr.trim().to_string())
    }
}

/// Merge a branch (e.g. task worktree branch) into current branch of repo.
pub fn git_merge_branch(
    cwd: &Path,
    worktree_dir: Option<&Path>,
    branch: &str,
) -> Result<String, String> {
    let repo = find_common_repo_root(cwd).ok_or_else(|| "Not a git repository".to_string())?;

    // If worktree dir has uncommitted changes, auto-commit them first so no work is lost
    if let Some(wt) = worktree_dir.filter(|p| p.is_dir()) {
        let status = Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["status", "--porcelain"])
            .output();
        if let Ok(out) = status {
            if !out.stdout.is_empty() {
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(wt)
                    .args(["add", "-A"])
                    .status();
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(wt)
                    .args(["commit", "-m", "Auto-commit task changes before merge"])
                    .status();
            }
        }
    }

    // Run merge in base repo
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["merge", branch, "--no-edit"])
        .output()
        .map_err(|e| format!("git merge failed: {e}"))?;

    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let summary = stdout.trim().to_string();
        Ok(if summary.is_empty() {
            format!("Merged {branch}")
        } else {
            summary
        })
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Abort failed merge to keep base checkout clean
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["merge", "--abort"])
            .status();
        let why = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        Err(format!("Merge failed: {why}"))
    }
}

/// Clean up a task worktree and its branch.
pub fn git_clean_worktree(
    cwd: &Path,
    worktree_dir: Option<&Path>,
    branch: Option<&str>,
) -> Result<(), String> {
    let repo = find_common_repo_root(cwd).ok_or_else(|| "Not a git repository".to_string())?;

    if let Some(wt) = worktree_dir {
        // Remove worktree via git
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "remove", "--force"])
            .arg(wt)
            .status();

        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "prune"])
            .status();

        if wt.exists() {
            let _ = std::fs::remove_dir_all(wt);
        }
    }

    // Delete the task branch if specified
    if let Some(b) = branch.filter(|b| !b.is_empty()) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "-D", b])
            .status();
    }

    Ok(())
}

pub fn find_repo_root(dir: &Path) -> Option<PathBuf> {
    for d in dir.ancestors().take(24) {
        let dot_git = d.join(".git");
        if dot_git.is_dir() || dot_git.is_file() {
            return Some(d.to_path_buf());
        }
    }
    None
}

pub fn find_common_repo_root(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return find_repo_root(dir);
    }
    let common_dir_str = String::from_utf8(out.stdout).ok()?.trim().to_string();
    let p = PathBuf::from(common_dir_str);
    let abs_common = if p.is_absolute() { p } else { dir.join(p) };
    abs_common
        .parent()
        .map(|p| p.to_path_buf())
        .or_else(|| find_repo_root(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("muqun-git-test-{name}-{stamp}"));
        let _ = std::fs::create_dir_all(&path);
        path
    }

    fn init_repo(dir: &Path) {
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-b", "main"])
            .status()
            .unwrap()
            .success());
        let _ = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.email", "test@example.com"])
            .status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "user.name", "Test"])
            .status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "commit.gpgsign", "false"])
            .status();
        std::fs::write(dir.join("init.txt"), "init\n").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "init.txt"])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-m", "initial commit"])
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn git_commit_stages_and_commits_changes() {
        let dir = temp_test_dir("commit");
        init_repo(&dir);

        // Empty message fails
        let empty_res = git_commit(&dir, "");
        assert!(empty_res.is_err());

        // Add a new file
        std::fs::write(dir.join("feature.txt"), "feature content\n").unwrap();
        let res = git_commit(&dir, "feat: add feature file").unwrap();
        assert!(res.contains("feat: add feature file") || res.contains("main"));

        // Verify git log has the commit
        let log = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["log", "-1", "--oneline"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("feat: add feature file"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_merge_branch_merges_feature_branch() {
        let dir = temp_test_dir("merge");
        init_repo(&dir);

        // Create feature branch
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["checkout", "-b", "feature-x"])
            .status()
            .unwrap()
            .success());

        std::fs::write(dir.join("feat_x.txt"), "feature x\n").unwrap();
        git_commit(&dir, "feat: implement x").unwrap();

        // Switch back to main
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["checkout", "main"])
            .status()
            .unwrap()
            .success());
        assert!(!dir.join("feat_x.txt").exists());

        // Merge feature-x
        let merge_res = git_merge_branch(&dir, None, "feature-x").unwrap();
        assert!(!merge_res.is_empty());
        assert!(dir.join("feat_x.txt").is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_clean_worktree_removes_worktree_and_branch() {
        let dir = temp_test_dir("clean-wt");
        init_repo(&dir);

        let wt_path = dir.parent().unwrap().join(format!(
            "muqun-wt-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        // Create linked worktree
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["worktree", "add", "-b", "muqun/test-wt"])
            .arg(&wt_path)
            .status()
            .unwrap()
            .success());
        assert!(wt_path.is_dir());

        // Clean worktree
        git_clean_worktree(&dir, Some(&wt_path), Some("muqun/test-wt")).unwrap();
        assert!(!wt_path.exists());

        // Verify branch is deleted
        let branch_check = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["branch", "--list", "muqun/test-wt"])
            .output()
            .unwrap();
        assert!(branch_check.stdout.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
