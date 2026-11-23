//! Goal-driven coordination over the existing task runtime. The coordinator
//! chooses its own work; execution observations never imply accepted quality.
pub mod router;
mod routes;
mod store;
#[cfg(test)]
mod tests;

pub(crate) use routes::{mount, spawn_reconciler};
pub(crate) use store::MIGRATION;
pub(crate) const CAPABILITY: &str = "agent_orchestration_preview_v1";
pub(crate) const POOL_CAPABILITY: &str = "agent_orchestration_pool_v1";
pub(crate) const ROUTER_CAPABILITY: &str = "agent_orchestration_router_v1";

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerSpec {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Start {
    client_ref: String,
    goal: String,
    cwd: String,
    coordinator: String,
    model: Option<String>,
    variant: Option<String>,
    #[serde(default)]
    asid: Option<String>,
    #[serde(default)]
    attachments: Vec<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reviewer: Option<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_review_rounds: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) computer_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) mentions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_approve: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) agent_pool: Vec<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_failover: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineStep {
    pub kind: String,
    pub round: u8,
    pub title: String,
    pub detail: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub time_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityGateView {
    pub id: String,
    pub status: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub severity: String,
    pub location: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunState {
    pub round: u8,
    pub max_rounds: u8,
    pub mao_phase: String,
    pub accepted: bool,
    pub coder_asid: Option<String>,
    pub reviewer_asid: Option<String>,
    pub reviewer_task_id: Option<String>,
    pub reviewed_round: u8,
    pub base_commit: Option<String>,
    pub base_commits: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub base_worktree_fingerprints: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_diff_fingerprint: Option<String>,
    pub files: Vec<ChangedFile>,
    pub coder_summary: Option<String>,
    pub review_summary: Option<String>,
    pub verdict: Option<String>,
    pub findings: Vec<ReviewFinding>,
    pub steps: Vec<PipelineStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_coder: Option<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_reviewer: Option<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exhausted_candidates: Vec<String>,
    #[serde(default)]
    pub failover_count: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_coder_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_decision: Option<router::RouteDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_asid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor_summary: Option<String>,
    #[serde(default)]
    pub skip_review: bool,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            round: 1,
            max_rounds: 2,
            mao_phase: "coding".into(),
            accepted: false,
            coder_asid: None,
            reviewer_asid: None,
            reviewer_task_id: None,
            reviewed_round: 0,
            base_commit: None,
            base_commits: std::collections::BTreeMap::new(),
            base_worktree_fingerprints: std::collections::BTreeMap::new(),
            last_diff_fingerprint: None,
            files: Vec::new(),
            coder_summary: None,
            review_summary: None,
            verdict: None,
            findings: Vec::new(),
            steps: Vec::new(),
            active_coder: None,
            active_reviewer: None,
            exhausted_candidates: Vec::new(),
            failover_count: 0,
            original_coder_title: None,
            route_decision: None,
            advisor_agent: None,
            advisor_asid: None,
            advisor_task_id: None,
            advisor_summary: None,
            skip_review: false,
        }
    }
}

pub fn candidate_key(spec: &ReviewerSpec) -> String {
    format!(
        "{}|{}|{}|{}",
        spec.agent.trim().to_ascii_lowercase(),
        spec.model
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
        spec.variant
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
        spec.machine
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
    )
}

pub fn candidate_display(spec: &ReviewerSpec) -> String {
    let mut out = spec.agent.clone();
    if let Some(model) = spec.model.as_deref().filter(|m| !m.trim().is_empty()) {
        out.push_str(" (");
        out.push_str(model.trim());
        if let Some(variant) = spec.variant.as_deref().filter(|v| !v.trim().is_empty()) {
            out.push_str(" · ");
            out.push_str(variant.trim());
        }
        out.push(')');
    }
    if let Some(machine) = spec.machine.as_deref().filter(|m| !m.trim().is_empty()) {
        out.push_str(" @ ");
        out.push_str(machine.trim());
    }
    out
}

pub fn is_quota_or_capacity_error(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "rate limit",
        "rate_limit",
        "ratelimit",
        "too many requests",
        "http 429",
        "status 429",
        "error 429",
        "429 ",
        "quota",
        "insufficient_quota",
        "usage limit",
        "limit reached",
        "out of credits",
        "no credits",
        "credit balance",
        "credits exhausted",
        "exhausted",
        "spend limit",
        "spend control",
        "billing limit",
        "overloaded",
        "capacity exceeded",
        "at capacity",
        "try again later",
        "cooldown",
        "额度",
        "配额",
        "限流",
        "达到上限",
        "用量已满",
        "余额不足",
        "次數上限",
        "額度",
        "配額",
    ];
    PATTERNS
        .iter()
        .any(|needle| lower.contains(needle) || trimmed.contains(needle))
}

fn bare_model_name(model: &str) -> &str {
    model
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(model)
        .trim()
}

pub fn is_account_limits_exhausted(
    limits: &crate::agents::domain::account_limits::AccountLimits,
    target_model: Option<&str>,
) -> Option<(String, bool)> {
    if limits.status != "available" {
        return None;
    }
    if limits.ordinary_usage_allowed == Some(false) {
        return Some(("account usage limit reached".into(), true));
    }
    let target_bare = target_model
        .map(bare_model_name)
        .filter(|m| !m.is_empty())
        .map(str::to_ascii_lowercase);
    for limit in &limits.limits {
        let model_scoped = limit
            .model_id
            .as_deref()
            .map(bare_model_name)
            .filter(|m| !m.is_empty());
        if let (Some(limit_model), Some(target)) = (model_scoped, target_bare.as_deref()) {
            let lm = limit_model.to_ascii_lowercase();
            if lm != target && !lm.contains(target) && !target.contains(&lm) {
                continue;
            }
        }
        let agent_wide = model_scoped.is_none();
        if limit.spend_control_reached == Some(true) {
            return Some((
                limit
                    .limit_reached_reason
                    .clone()
                    .unwrap_or_else(|| "spend control limit reached".into()),
                agent_wide,
            ));
        }
        if let Some(reason) = limit
            .limit_reached_reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
        {
            return Some((reason.to_string(), agent_wide));
        }
        if limit
            .individual_limit
            .as_ref()
            .and_then(|il| il.remaining_percent)
            .is_some_and(|pct| pct <= 0.5)
        {
            return Some(("individual quota exhausted".into(), agent_wide));
        }
        // Only treat missing extra credits as exhausted when the provider reports no
        // included rate windows at all (subscription accounts routinely have
        // `has_credits: false` for optional overage credits while included windows
        // still have remaining capacity).
        if limit.windows.is_empty()
            && limit
                .credits
                .as_ref()
                .is_some_and(|c| c.has_credits == Some(false) && c.unlimited != Some(true))
        {
            return Some(("account credits exhausted".into(), agent_wide));
        }
        for w in &limit.windows {
            let label = w.label.as_deref().unwrap_or(&w.id);
            if w.remaining.is_some_and(|r| r <= 0.0) {
                return Some((format!("rate window `{label}` exhausted"), agent_wide));
            }
            if let (Some(used), Some(cap)) = (w.used, w.limit) {
                if cap > 0.0 && used >= cap {
                    return Some((format!("rate window `{label}` reached 100%"), agent_wide));
                }
            }
            if w.unit.eq_ignore_ascii_case("percent") && w.used.is_some_and(|u| u >= 99.5) {
                return Some((format!("rate window `{label}` reached 100%"), agent_wide));
            }
        }
    }
    None
}

pub fn format_agent_pool_instruction(pool: &[ReviewerSpec]) -> String {
    if pool.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(pool.len() + 2);
    lines.push(
        "User-configured agent & model pool for dynamic task orchestration (no fixed frontend/backend/architect role labels — decide based on the task itself whether to execute directly or delegate bounded subtasks via Muqun MCP `tasks.delegate`, picking the best suited agent/model below; if any candidate hits a quota or rate limit, switch to another candidate from this pool):"
            .to_string(),
    );
    for (idx, item) in pool.iter().take(12).enumerate() {
        let mut parts = vec![format!("agent=`{}`", item.agent)];
        if let Some(model) = item.model.as_deref().filter(|m| !m.trim().is_empty()) {
            parts.push(format!("model=`{}`", model.trim()));
        }
        if let Some(variant) = item.variant.as_deref().filter(|v| !v.trim().is_empty()) {
            parts.push(format!("variant=`{}`", variant.trim()));
        }
        if let Some(machine) = item.machine.as_deref().filter(|m| !m.trim().is_empty()) {
            parts.push(format!("machine=`{}`", machine.trim()));
        }
        lines.push(format!("{}. {}", idx + 1, parts.join(", ")));
    }
    lines.join("\n")
}

pub fn diff_fingerprint(files: &[ChangedFile], diff_summary: &str) -> String {
    let mut out = String::new();
    for f in files {
        out.push_str(&format!("{}:{}:{};", f.path, f.additions, f.deletions));
    }
    out.push_str(&format!("len={}", diff_summary.trim().len()));
    out
}

fn run_git_cmd(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn resolve_repo_root(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    if dir.join(".git").exists() {
        return dir.canonicalize().ok().or_else(|| Some(dir.to_path_buf()));
    }
    let root = run_git_cmd(dir, &["rev-parse", "--show-toplevel"])?;
    let p = std::path::PathBuf::from(root.trim());
    p.is_dir().then(|| p.canonicalize().ok().unwrap_or(p))
}

fn push_unique_repo(repos: &mut Vec<std::path::PathBuf>, candidate: std::path::PathBuf) {
    if repos.len() < 8 && !repos.iter().any(|existing| existing == &candidate) {
        repos.push(candidate);
    }
}

pub fn discover_git_repos(cwd: &str, goal: &str) -> Vec<std::path::PathBuf> {
    let mut repos = Vec::new();
    let trimmed = cwd.trim();
    if !trimmed.is_empty() {
        let base_dir = std::path::Path::new(trimmed);
        if let Some(root) = resolve_repo_root(base_dir) {
            push_unique_repo(&mut repos, root);
        }
        if base_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(base_dir) {
                let mut subdirs: Vec<std::path::PathBuf> = entries
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name();
                        let name = name.to_str()?;
                        if name.starts_with('.') || matches!(name, "target" | "node_modules") {
                            return None;
                        }
                        let path = entry.path();
                        (path.is_dir() && path.join(".git").exists()).then_some(path)
                    })
                    .collect();
                subdirs.sort();
                for sub in subdirs.into_iter().take(24) {
                    if let Some(root) = resolve_repo_root(&sub) {
                        push_unique_repo(&mut repos, root);
                    }
                }
            }
        }
    }

    for token in goal.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '`'
            )
    }) {
        let cleaned = token.trim_end_matches(['.', ':', '!']);
        if cleaned.starts_with('/') && cleaned.len() > 1 {
            let p = std::path::Path::new(cleaned);
            if p.exists() {
                let target_dir = if p.is_file() {
                    p.parent().unwrap_or(p)
                } else {
                    p
                };
                if let Some(root) = resolve_repo_root(target_dir) {
                    push_unique_repo(&mut repos, root);
                }
            }
        }
    }

    repos
}

pub fn repo_worktree_fingerprint(repo_dir: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let head = run_git_cmd(repo_dir, &["rev-parse", "--verify", "HEAD"]).unwrap_or_default();
    let status = run_git_cmd(repo_dir, &["status", "--porcelain"]).unwrap_or_default();
    let diff = run_git_cmd(repo_dir, &["diff", "--no-color"]).unwrap_or_default();
    let cached = run_git_cmd(repo_dir, &["diff", "--cached", "--no-color"]).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(head.trim().as_bytes());
    hasher.update(b"\n--status--\n");
    hasher.update(status.as_bytes());
    hasher.update(b"\n--diff--\n");
    hasher.update(diff.as_bytes());
    hasher.update(b"\n--cached--\n");
    hasher.update(cached.as_bytes());
    let digest = hasher.finalize();
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn capture_worktree_fingerprints(
    cwd: &str,
    goal: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for repo in discover_git_repos(cwd, goal) {
        if let Some(fp) = repo_worktree_fingerprint(&repo) {
            map.insert(repo.to_string_lossy().into_owned(), fp);
        }
    }
    map
}

pub fn capture_base_commits(cwd: &str, goal: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for repo in discover_git_repos(cwd, goal) {
        if let Some(sha) = run_git_cmd(&repo, &["rev-parse", "--verify", "HEAD"]) {
            let sha = sha.trim().to_string();
            if !sha.is_empty() {
                map.insert(repo.to_string_lossy().into_owned(), sha);
            }
        }
    }
    map
}

pub fn collect_local_repos_diff(
    cwd: &str,
    goal: &str,
    base_commits: &std::collections::BTreeMap<String, String>,
    legacy_base: Option<&str>,
) -> (Vec<ChangedFile>, String) {
    let empty = std::collections::BTreeMap::new();
    collect_local_repos_diff_since(cwd, goal, base_commits, &empty, legacy_base)
}

pub fn collect_local_repos_diff_since(
    cwd: &str,
    goal: &str,
    base_commits: &std::collections::BTreeMap<String, String>,
    base_worktree_fingerprints: &std::collections::BTreeMap<String, String>,
    legacy_base: Option<&str>,
) -> (Vec<ChangedFile>, String) {
    let mut repos: Vec<std::path::PathBuf> = Vec::new();
    for path_str in base_commits.keys() {
        if let Some(root) = resolve_repo_root(std::path::Path::new(path_str)) {
            push_unique_repo(&mut repos, root);
        }
    }
    for discovered in discover_git_repos(cwd, goal) {
        push_unique_repo(&mut repos, discovered);
    }
    if repos.is_empty() {
        return (Vec::new(), String::new());
    }

    let trimmed_cwd = cwd.trim();
    let canonical_cwd = (!trimmed_cwd.is_empty()).then(|| {
        std::path::Path::new(trimmed_cwd)
            .canonicalize()
            .ok()
            .unwrap_or_else(|| std::path::PathBuf::from(trimmed_cwd))
    });
    let multi_repo = repos.len() > 1
        || repos
            .first()
            .is_some_and(|first| canonical_cwd.as_ref().is_none_or(|c| c != first));

    let mut all_files: Vec<ChangedFile> = Vec::new();
    let mut combined = String::new();
    let total_budget = 8_000usize;

    for (idx, repo_dir) in repos.iter().enumerate() {
        let key = repo_dir.to_string_lossy();
        if let Some(before_fp) = base_worktree_fingerprints.get(key.as_ref()) {
            if repo_worktree_fingerprint(repo_dir).as_deref() == Some(before_fp.as_str()) {
                // Repository working tree & HEAD are byte-for-byte identical to pre-turn
                // snapshot: do not misattribute pre-existing uncommitted files to this turn.
                continue;
            }
        }
        let base = base_commits
            .get(key.as_ref())
            .map(String::as_str)
            .or(if idx == 0 { legacy_base } else { None });
        let mut numstat_args = vec!["diff", "--numstat"];
        let mut stat_args = vec!["diff", "--stat"];
        let mut raw_args = vec!["diff", "--no-color"];
        if let Some(rev) = base {
            numstat_args.push(rev);
            stat_args.push(rev);
            raw_args.push(rev);
        }
        let numstat = run_git_cmd(repo_dir, &numstat_args).unwrap_or_default();
        let status = run_git_cmd(repo_dir, &["status", "--porcelain"]).unwrap_or_default();
        let stat = run_git_cmd(repo_dir, &stat_args).unwrap_or_default();
        let raw = run_git_cmd(repo_dir, &raw_args).unwrap_or_default();

        let mut parsed: Vec<ChangedFile> = Vec::new();
        for line in numstat.lines().take(32) {
            let mut parts = line.split('\t');
            if let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) {
                parsed.push(ChangedFile {
                    path: path.trim().to_string(),
                    additions: a.trim().parse().unwrap_or(0),
                    deletions: d.trim().parse().unwrap_or(0),
                });
            }
        }
        let mut untracked_notes = Vec::new();
        for line in status.lines().take(32) {
            if let Some(rest) = line.strip_prefix("?? ") {
                let clean = rest.trim().trim_matches('"');
                if !clean.is_empty() && !parsed.iter().any(|f| f.path == clean) {
                    let full = repo_dir.join(clean);
                    let lines = std::fs::metadata(&full)
                        .ok()
                        .filter(|m| m.is_file() && m.len() <= 131_072)
                        .and_then(|_| std::fs::read_to_string(&full).ok())
                        .map(|body| body.lines().count())
                        .unwrap_or(0);
                    parsed.push(ChangedFile {
                        path: clean.to_string(),
                        additions: lines,
                        deletions: 0,
                    });
                    untracked_notes.push(format!("- {clean} (new file, +{lines} lines)"));
                }
            }
        }
        if parsed.is_empty() {
            for line in status.lines().take(32) {
                if line.len() >= 4 {
                    let clean = line[3..].trim().trim_matches('"');
                    if !clean.is_empty() {
                        parsed.push(ChangedFile {
                            path: clean.to_string(),
                            additions: 0,
                            deletions: 0,
                        });
                    }
                }
            }
        }

        if parsed.is_empty() && status.trim().is_empty() && raw.trim().is_empty() {
            continue;
        }

        let repo_name = repo_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo");
        for mut file in parsed {
            if multi_repo {
                file.path = format!("{repo_name}/{}", file.path);
            }
            all_files.push(file);
            if all_files.len() >= 48 {
                break;
            }
        }

        if multi_repo {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&format!(
                "=== Repository: {repo_name} ({}) ===\n",
                repo_dir.display()
            ));
        }
        if !status.trim().is_empty() {
            combined.push_str("Git status:\n");
            combined.push_str(status.trim());
            combined.push('\n');
        }
        if !stat.trim().is_empty() {
            combined.push_str("\nGit diff --stat:\n");
            combined.push_str(stat.trim());
            combined.push('\n');
        }
        if !untracked_notes.is_empty() {
            combined.push_str("\nNew untracked files:\n");
            combined.push_str(&untracked_notes.join("\n"));
            combined.push('\n');
        }
        if !raw.trim().is_empty() && combined.len() < total_budget {
            let remaining = total_budget.saturating_sub(combined.len());
            let mut cut = raw.len().min(remaining);
            while cut > 0 && !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            if cut > 0 {
                combined.push_str("\nGit diff excerpt:\n");
                combined.push_str(&raw[..cut]);
                combined.push('\n');
            }
        }
    }

    (all_files, combined)
}

pub fn extract_timeline_evidence<'a, I>(parts: I) -> (Vec<ChangedFile>, String)
where
    I: IntoIterator<Item = &'a crate::agents::domain::timeline::AgentPart>,
{
    use crate::agents::domain::{AgentPart, ToolCallStatus};
    use std::collections::BTreeMap;

    let mut files_map: BTreeMap<String, ChangedFile> = BTreeMap::new();
    let mut tool_lines: Vec<String> = Vec::new();
    let mut diff_snippets: Vec<String> = Vec::new();
    let mut snippet_bytes = 0usize;

    for part in parts {
        match part {
            AgentPart::Tool(tc) => {
                if let Some(meta_files) = tc
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("files"))
                    .and_then(|v| v.as_array())
                {
                    for item in meta_files {
                        let Some(path) = item
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        else {
                            continue;
                        };
                        let additions =
                            item.get("additions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let deletions =
                            item.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let entry =
                            files_map
                                .entry(path.to_string())
                                .or_insert_with(|| ChangedFile {
                                    path: path.to_string(),
                                    additions: 0,
                                    deletions: 0,
                                });
                        entry.additions = entry.additions.max(additions);
                        entry.deletions = entry.deletions.max(deletions);

                        if snippet_bytes < 5000 {
                            if let Some(diff) = item.get("diff").and_then(|v| v.as_str()) {
                                let trimmed = diff.trim();
                                if !trimmed.is_empty() {
                                    let excerpt = concise_excerpt(trimmed, 1200);
                                    snippet_bytes += excerpt.len();
                                    diff_snippets.push(format!("--- {path}\n{excerpt}"));
                                }
                            }
                        }
                    }
                }

                if matches!(tc.state, ToolCallStatus::Completed | ToolCallStatus::Failed)
                    && tool_lines.len() < 24
                {
                    let status_label = match tc.state {
                        ToolCallStatus::Completed => "ok",
                        ToolCallStatus::Failed => "failed",
                        _ => "pending",
                    };
                    let cmd_or_title = tc
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("command").and_then(|v| v.as_str()))
                        .or_else(|| tc.input.get("command").and_then(|v| v.as_str()))
                        .or_else(|| tc.input.get("goal").and_then(|v| v.as_str()))
                        .or(tc.title.as_deref())
                        .unwrap_or("");
                    let exit_note = tc
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("exitCode").or_else(|| m.get("exit")))
                        .and_then(|v| v.as_i64())
                        .map(|code| format!(" exit={code}"))
                        .unwrap_or_default();
                    let out_str = tc
                        .output
                        .as_ref()
                        .and_then(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .or_else(|| Some(v.to_string()))
                        })
                        .unwrap_or_default();
                    let detail = concise_excerpt(cmd_or_title, 140);
                    let out_excerpt = if out_str.trim().is_empty() {
                        String::new()
                    } else {
                        format!(" => {}", concise_excerpt(out_str.trim(), 180))
                    };
                    tool_lines.push(format!(
                        "- {} [{}{}]{}{}",
                        tc.name,
                        status_label,
                        exit_note,
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {detail}")
                        },
                        out_excerpt
                    ));
                }
            }
            AgentPart::Diff { file, diff } => {
                let file_clean = file.trim();
                if file_clean.is_empty() {
                    if diff_snippets.is_empty() && snippet_bytes < 5000 && !diff.trim().is_empty() {
                        let excerpt = concise_excerpt(diff.trim(), 1200);
                        snippet_bytes += excerpt.len();
                        diff_snippets.push(excerpt);
                    }
                    continue;
                }
                let mut additions = 0usize;
                let mut deletions = 0usize;
                for line in diff.lines() {
                    if line.starts_with("+++") || line.starts_with("---") {
                        continue;
                    }
                    if line.starts_with('+') {
                        additions += 1;
                    } else if line.starts_with('-') {
                        deletions += 1;
                    }
                }
                let entry =
                    files_map
                        .entry(file_clean.to_string())
                        .or_insert_with(|| ChangedFile {
                            path: file_clean.to_string(),
                            additions: 0,
                            deletions: 0,
                        });
                entry.additions = entry.additions.max(additions);
                entry.deletions = entry.deletions.max(deletions);
                if snippet_bytes < 5000 && !diff.trim().is_empty() {
                    let excerpt = concise_excerpt(diff.trim(), 1200);
                    snippet_bytes += excerpt.len();
                    diff_snippets.push(format!("--- {file_clean}\n{excerpt}"));
                }
            }
            AgentPart::Shell {
                command,
                status,
                exit,
                output,
                ..
            } if tool_lines.len() < 24 => {
                let exit_note = exit
                    .map(|code| format!(" exit={}", code as i64))
                    .unwrap_or_default();
                let out_excerpt = output
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" => {}", concise_excerpt(s, 180)))
                    .unwrap_or_default();
                tool_lines.push(format!(
                    "- shell [{}{}]: {}{}",
                    status,
                    exit_note,
                    concise_excerpt(command, 140),
                    out_excerpt
                ));
            }
            _ => {}
        }
    }

    let mut summary = String::new();
    if !tool_lines.is_empty() || !diff_snippets.is_empty() {
        summary.push_str("=== Session Tool & MCP Execution Evidence ===\n");
        if !tool_lines.is_empty() {
            summary.push_str(&tool_lines.join("\n"));
            summary.push('\n');
        }
        if !diff_snippets.is_empty() {
            summary.push_str("\nTool Diff Excerpt:\n");
            summary.push_str(&diff_snippets.join("\n"));
            summary.push('\n');
        }
    }
    let files: Vec<ChangedFile> = files_map.into_values().take(48).collect();
    (files, summary)
}

pub fn normalize_changed_path(cwd: &str, path: &str) -> String {
    let mut clean = path.trim().trim_start_matches("./");
    let base = cwd.trim().trim_end_matches(['/', '\\']);
    if !base.is_empty() {
        if let Some(rest) = clean.strip_prefix(base) {
            let stripped = rest.trim_start_matches(['/', '\\']);
            if !stripped.is_empty() {
                clean = stripped;
            }
        }
    }
    clean.trim_start_matches("./").to_string()
}

pub fn merge_changed_files(cwd: &str, target: &mut Vec<ChangedFile>, incoming: Vec<ChangedFile>) {
    for item in incoming {
        let norm = normalize_changed_path(cwd, &item.path);
        if norm.is_empty() {
            continue;
        }
        if let Some(existing) = target
            .iter_mut()
            .find(|f| normalize_changed_path(cwd, &f.path) == norm)
        {
            existing.additions = existing.additions.max(item.additions);
            existing.deletions = existing.deletions.max(item.deletions);
        } else {
            target.push(ChangedFile {
                path: norm,
                additions: item.additions,
                deletions: item.deletions,
            });
        }
    }
}

impl RunState {
    fn for_request(request: &Start) -> Self {
        let now = crate::tasks::domain::now_ms();
        let base_commits = capture_base_commits(&request.cwd, &request.goal);
        let base_worktree_fingerprints = capture_worktree_fingerprints(&request.cwd, &request.goal);
        let base_commit = resolve_repo_root(std::path::Path::new(&request.cwd))
            .and_then(|root| base_commits.get(&root.display().to_string()).cloned())
            .or_else(|| base_commits.values().next().cloned());
        let active_coder = Some(ReviewerSpec {
            agent: request.coordinator.clone(),
            model: request.model.clone(),
            variant: request.variant.clone(),
            machine: request.machine.clone(),
        });
        let active_reviewer = request.reviewer.clone();
        let initial_step = PipelineStep {
            kind: "coding".into(),
            round: 1,
            title: format!("Coding #1 · {}", request.coordinator),
            detail: concise_excerpt(&request.goal, 180),
            status: "running".into(),
            asid: request.asid.clone(),
            task_id: None,
            time_ms: now,
        };
        Self {
            round: 1,
            max_rounds: request.max_review_rounds.unwrap_or(2).clamp(1, 5),
            mao_phase: "coding".into(),
            accepted: false,
            coder_asid: request.asid.clone(),
            reviewer_asid: None,
            reviewer_task_id: None,
            reviewed_round: 0,
            base_commit,
            base_commits,
            base_worktree_fingerprints,
            last_diff_fingerprint: None,
            files: Vec::new(),
            coder_summary: None,
            review_summary: None,
            verdict: None,
            findings: Vec::new(),
            steps: vec![initial_step],
            active_coder,
            active_reviewer,
            exhausted_candidates: Vec::new(),
            failover_count: 0,
            original_coder_title: None,
            ..Default::default()
        }
    }

    pub fn quality_gates(&self) -> Vec<QualityGateView> {
        let total_add: usize = self.files.iter().map(|f| f.additions).sum();
        let total_del: usize = self.files.iter().map(|f| f.deletions).sum();

        let coding = match self.mao_phase.as_str() {
            "coding" => QualityGateView {
                id: "coding".into(),
                status: "running".into(),
                detail: format!("Round #{}", self.round),
            },
            "failed" => QualityGateView {
                id: "coding".into(),
                status: "failed".into(),
                detail: "Failed".into(),
            },
            "cancelled" => QualityGateView {
                id: "coding".into(),
                status: "idle".into(),
                detail: "Cancelled".into(),
            },
            _ => QualityGateView {
                id: "coding".into(),
                status: "completed".into(),
                detail: "Completed".into(),
            },
        };

        let diff = if !self.files.is_empty() {
            QualityGateView {
                id: "changeset".into(),
                status: "completed".into(),
                detail: format!(
                    "{} file(s) (+{} -{})",
                    self.files.len(),
                    total_add,
                    total_del
                ),
            }
        } else if matches!(self.mao_phase.as_str(), "reviewing" | "approved") {
            QualityGateView {
                id: "changeset".into(),
                status: "completed".into(),
                detail: "Captured".into(),
            }
        } else {
            QualityGateView {
                id: "changeset".into(),
                status: "idle".into(),
                detail: "Pending".into(),
            }
        };

        let review = match self.mao_phase.as_str() {
            "reviewing" => QualityGateView {
                id: "review".into(),
                status: "running".into(),
                detail: format!("Round {}/{}", self.round, self.max_rounds),
            },
            "approved" => QualityGateView {
                id: "review".into(),
                status: "completed".into(),
                detail: format!("Approved #{}", self.round),
            },
            "changes_requested" => QualityGateView {
                id: "review".into(),
                status: "warning".into(),
                detail: format!("Changes requested #{}", self.round),
            },
            "coding" if self.round > 1 => QualityGateView {
                id: "review".into(),
                status: "warning".into(),
                detail: format!("Rework #{}", self.round),
            },
            _ => QualityGateView {
                id: "review".into(),
                status: "idle".into(),
                detail: "Pending".into(),
            },
        };

        let quality = match self.mao_phase.as_str() {
            "approved" => QualityGateView {
                id: "quality".into(),
                status: "completed".into(),
                detail: "Passed".into(),
            },
            "changes_requested" => QualityGateView {
                id: "quality".into(),
                status: "warning".into(),
                detail: "Needs rework".into(),
            },
            "failed" => QualityGateView {
                id: "quality".into(),
                status: "failed".into(),
                detail: "Failed".into(),
            },
            "cancelled" => QualityGateView {
                id: "quality".into(),
                status: "idle".into(),
                detail: "Cancelled".into(),
            },
            _ => QualityGateView {
                id: "quality".into(),
                status: "idle".into(),
                detail: "Pending".into(),
            },
        };

        vec![coding, diff, review, quality]
    }
}

pub(crate) fn concise_excerpt(text: &str, max_chars: usize) -> String {
    let clean = text.trim();
    match clean.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", clean[..idx].trim_end()),
        None => clean.to_string(),
    }
}

pub(crate) fn parse_findings(reply: &str) -> Vec<ReviewFinding> {
    let mut findings = Vec::new();
    for raw in reply.lines() {
        let trimmed = raw
            .trim()
            .trim_start_matches('-')
            .trim_start_matches('*')
            .trim();
        let upper = trimmed.to_ascii_uppercase();
        let Some(pos) = upper.find("MAO_ISSUE:") else {
            continue;
        };
        let mut rest = trimmed[pos + "MAO_ISSUE:".len()..].trim();
        let mut severity = "medium".to_string();
        if let Some(after_open) = rest.strip_prefix('[') {
            if let Some(close) = after_open.find(']') {
                let tag = after_open[..close].trim().to_ascii_uppercase();
                severity = match tag.as_str() {
                    "HIGH" | "CRITICAL" | "BLOCKER" => "high",
                    "LOW" | "NIT" | "INFO" => "low",
                    _ => "medium",
                }
                .to_string();
                rest = after_open[close + 1..].trim();
            }
        }
        let (location, message) = if let Some((loc, msg)) = rest.split_once('|') {
            (loc.trim(), msg.trim())
        } else if let Some((loc, msg)) = rest.split_once('—') {
            (loc.trim(), msg.trim())
        } else if let Some((loc, msg)) = rest.split_once(" - ") {
            (loc.trim(), msg.trim())
        } else {
            ("workspace", rest)
        };
        if !message.is_empty() {
            findings.push(ReviewFinding {
                severity,
                location: if location.is_empty() {
                    "workspace".into()
                } else {
                    location.to_string()
                },
                message: concise_excerpt(message, 220),
            });
        }
        if findings.len() >= 16 {
            break;
        }
    }
    findings
}

pub(crate) fn clean_review_summary(reply: &str, max_chars: usize) -> String {
    let lines: Vec<&str> = reply
        .lines()
        .filter(|line| {
            let upper = line.trim().to_ascii_uppercase();
            !upper.contains("MAO_VERDICT:") && !upper.contains("MAO_ISSUE:")
        })
        .collect();
    let cleaned = lines.join("\n");
    concise_excerpt(&cleaned, max_chars)
}

pub(crate) fn parse_verdict(reply: &str) -> (&'static str, String) {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return ("approved", String::new());
    }
    let mut explicit: Option<bool> = None;
    for raw in trimmed.lines() {
        let upper = raw.trim().to_ascii_uppercase();
        if let Some(pos) = upper.find("MAO_VERDICT:") {
            let tail = upper[pos + "MAO_VERDICT:".len()..].trim();
            if tail.starts_with("CHANGES_REQUESTED")
                || tail.starts_with("REQUEST_CHANGES")
                || tail.starts_with("REJECTED")
            {
                explicit = Some(false);
            } else if tail.starts_with("APPROVED") || tail.starts_with("PASS") {
                explicit = Some(true);
            }
        }
    }
    let excerpt = clean_review_summary(trimmed, 800);
    let findings = parse_findings(trimmed);
    let low_only = !findings.is_empty() && findings.iter().all(|f| f.severity == "low");
    match explicit {
        Some(true) => ("approved", excerpt),
        Some(false) if low_only => ("approved", excerpt),
        Some(false) => ("changes_requested", excerpt),
        None => {
            let lower = trimmed.to_ascii_lowercase();
            let negative = [
                "changes requested",
                "changes_requested",
                "must fix",
                "blocking issue",
                "blocker:",
                "need to fix",
                "issues found",
                "需要修改",
                "存在问题",
                "不通过",
                "打回修改",
            ];
            if !low_only
                && negative
                    .iter()
                    .any(|needle| lower.contains(needle) || trimmed.contains(needle))
            {
                ("changes_requested", excerpt)
            } else {
                ("approved", excerpt)
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Run {
    id: String,
    owner: String,
    request: Start,
    created_ms: u64,
    task_id: Option<String>,
    #[serde(default)]
    state: RunState,
}

impl Run {
    fn task_ref(&self) -> String {
        format!("orchestration:{}", self.id)
    }

    fn effective_coder(&self) -> ReviewerSpec {
        self.state
            .active_coder
            .clone()
            .unwrap_or_else(|| ReviewerSpec {
                agent: self.request.coordinator.clone(),
                model: self.request.model.clone(),
                variant: self.request.variant.clone(),
                machine: self.request.machine.clone(),
            })
    }

    fn effective_reviewer(&self) -> Option<ReviewerSpec> {
        self.state
            .active_reviewer
            .clone()
            .or_else(|| self.request.reviewer.clone())
    }

    fn auto_failover_enabled(&self) -> bool {
        self.request
            .auto_failover
            .unwrap_or(!self.request.agent_pool.is_empty())
    }

    fn coding_prompt(&self) -> String {
        self.request.goal.trim().to_string()
    }

    fn review_prompt(
        &self,
        round: u8,
        max_rounds: u8,
        coder_summary: &str,
        diff_summary: &str,
    ) -> String {
        let diff_block = if diff_summary.trim().is_empty() {
            "(No local git diff returned directly; check remote/MCP task evidence, session tool logs, or workspace files before concluding.)"
        } else {
            diff_summary.trim()
        };
        let cross_repo_rule = if self.state.base_commits.len() > 1 {
            "- Cross-repo consistency: verify API contracts, shared types, imports, and configuration changes match across every affected repository.\n"
        } else {
            ""
        };
        format!(
            "You are the designated **Read-Only Code Reviewer** in an automated Coder ⇄ Reviewer pipeline (Review Round {round}/{max_rounds}).\n\n\
             STRICT RULES:\n\
             - Read-only verifier: NEVER edit, write, delete, or commit workspace source files. Only inspect diffs/files/task outputs and run read-only verification checks or tests.\n\
             - Tasks in Muqun may modify single/multiple local git repos, external filesystem paths, OR remote linked computers via Muqun MCP (`tasks.delegate`, `exec.run`). NEVER assume no work happened just because local `git diff` is empty when MCP/remote execution evidence or tool logs show real changes or test results.\n\
             - NEVER ask interactive questions or wait for user confirmation; make a decisive verdict from the evidence and read-only checks.\n\
             {cross_repo_rule}\
             - Focus on correctness bugs, edge cases, regressions, security flaws, missing tests, or rule violations.\n\n\
             User task goal:\n{}\n\n\
             Coder summary:\n{coder_summary}\n\n\
             Changed files / Execution evidence:\n{diff_block}\n\n\
             Output format:\n\
             - Give a concise review summary first.\n\
             - For each concrete issue found, add one line formatted as:\n\
               `MAO_ISSUE: [HIGH|MEDIUM|LOW] path:line | clear issue description and expected fix`\n\
             - Your final line MUST start with exactly one of:\n\
               - `MAO_VERDICT: APPROVED` — if clean and ready.\n\
               - `MAO_VERDICT: CHANGES_REQUESTED` — if issues must be fixed.",
            self.request.goal
        )
    }

    fn prompt(&self) -> String {
        let reviewer_section = if let Some(ref rev) = self.request.reviewer {
            let mut details = format!("agent `{}`", rev.agent);
            if let Some(ref model) = rev.model {
                details.push_str(&format!(", model `{model}`"));
            }
            if let Some(ref variant) = rev.variant {
                details.push_str(&format!(", reasoning effort `{variant}`"));
            }
            let rounds = self.request.max_review_rounds.unwrap_or(2);
            format!(
                "\nDesignated Reviewer: {details}.\n\
                 Once implementation and verification finish, delegate a code review of the changed files and diff to the designated reviewer via tasks.delegate (passing the exact agent_id, model, and variant above).\n\
                 If the reviewer requests changes, fix the issues and re-request review for up to {rounds} review round(s)."
            )
        } else {
            String::new()
        };

        let pool_section = if !self.request.agent_pool.is_empty() {
            format!(
                "\n\n{}",
                format_agent_pool_instruction(&self.request.agent_pool)
            )
        } else {
            String::new()
        };

        format!(
            "You coordinate this software task through the Muqun task tools.\n\
             Investigate before deciding whether to delegate. Dynamically choose the work, agents,\n\
             models and dependencies from actual findings; no fixed team or task template.\n\
             Use fleet.describe to discover available agents. Delegate bounded tasks through\n\
             tasks.delegate, retaining each returned task_id. Give each worker the goal, allowed\n\
             scope, constraints, required deliverables and observable acceptance criteria.\n\
             Use tasks.read/tasks.wait to inspect actual results before scheduling dependent work.\n\
             Use exec.run for verification so exit status and output are recorded as tasks.\n\
             A worker being idle or saying done is not proof. Inspect changes and verification\n\
             evidence. Review and test the same revision; revalidate after integration.\n\
             Keep writes serial unless workers have isolated worktrees. Preserve existing changes.\n\
             Do not expand the user's permissions, answer trust prompts, publish, merge to main,\n\
             or repeat a submission whose delivery is uncertain. Honor repository instructions.\n\
             Stop repetitive failed approaches and explain the blocker instead of looping.\n\
             Finish with the changed files/revisions, verification task IDs, results, and remaining\n\
             gaps. The host presents your result for review, not as automatically accepted.{reviewer_section}{pool_section}\n\n\
             User goal:\n{}",
            self.request.goal
        )
    }
}
