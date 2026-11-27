//! Paginated pull request listing and checkout for an agent session's checkout.
//!
//! Supports GitHub via `gh api` (REST pulls endpoint with `Link` header
//! pagination for `author=all`, and Search Issues API for `author=me`) and
//! Gitea via `tea pulls list`. All external CLI invocations use bounded
//! timeouts, `kill_on_drop`, scrubbed git environment, and a 30-second
//! in-memory TTL cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::platform::git::{self, GitError, RemoteInfo};

const PR_CLI_TIMEOUT: Duration = Duration::from_secs(15);
const PR_CHECKOUT_TIMEOUT: Duration = Duration::from_secs(30);
const PR_CACHE_TTL: Duration = Duration::from_secs(30);
const PR_CACHE_MAX_ENTRIES: usize = 256;
const MAX_CLI_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CLI_STDERR_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestItem {
    pub number: u64,
    pub title: String,
    /// `"open"`, `"closed"`, or `"merged"`.
    pub state: String,
    pub draft: bool,
    pub url: String,
    pub author: String,
    pub head_branch: String,
    pub base_branch: String,
    pub updated_at: String,
    /// `"success"`, `"pending"`, `"failure"`, or `None`.
    pub checks_status: Option<String>,
}

impl PullRequestItem {
    pub fn to_json(&self) -> Value {
        json!({
            "number": self.number,
            "title": self.title,
            "state": self.state,
            "draft": self.draft,
            "url": self.url,
            "author": self.author,
            "head_branch": self.head_branch,
            "base_branch": self.base_branch,
            "updated_at": self.updated_at,
            "checks_status": self.checks_status,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestsPage {
    pub host: String,
    pub supported: bool,
    pub reason: Option<String>,
    pub repo_url: Option<String>,
    pub items: Vec<PullRequestItem>,
    pub page: u32,
    pub next_page: Option<u32>,
    pub has_more: bool,
}

impl PullRequestsPage {
    pub fn to_json(&self) -> Value {
        json!({
            "host": self.host,
            "supported": self.supported,
            "reason": self.reason,
            "repo_url": self.repo_url,
            "items": self.items.iter().map(PullRequestItem::to_json).collect::<Vec<_>>(),
            "page": self.page,
            "next_page": self.next_page,
            "has_more": self.has_more,
        })
    }

    pub fn unsupported(
        host: impl Into<String>,
        reason: impl Into<String>,
        repo_url: Option<String>,
        page: u32,
    ) -> Self {
        Self {
            host: host.into(),
            supported: false,
            reason: Some(reason.into()),
            repo_url,
            items: Vec::new(),
            page: page.max(1),
            next_page: None,
            has_more: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    toplevel: PathBuf,
    state: String,
    author: String,
    page: u32,
    limit: u32,
}

static PR_CACHE: OnceLock<Mutex<HashMap<CacheKey, (Instant, PullRequestsPage)>>> = OnceLock::new();

fn cache_get(key: &CacheKey, now: Instant) -> Option<PullRequestsPage> {
    let cache = PR_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = cache.lock().ok()?;
    guard
        .get(key)
        .filter(|(ts, _)| now.saturating_duration_since(*ts) < PR_CACHE_TTL)
        .map(|(_, page)| page.clone())
}

fn cache_put(key: CacheKey, page: PullRequestsPage, now: Instant) {
    let cache = PR_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = cache.lock() {
        guard.retain(|_, (ts, _)| now.saturating_duration_since(*ts) < PR_CACHE_TTL);
        if guard.len() >= PR_CACHE_MAX_ENTRIES {
            // Evict oldest entry to stay strictly bounded.
            if let Some(oldest) = guard
                .iter()
                .min_by_key(|(_, (ts, _))| *ts)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest);
            }
        }
        guard.insert(key, (now, page));
    }
}

pub fn normalize_pr_state(state: &str) -> &'static str {
    match state.trim().to_ascii_lowercase().as_str() {
        "closed" => "closed",
        "all" => "all",
        _ => "open",
    }
}

pub fn normalize_pr_author(author: &str) -> &'static str {
    match author.trim().to_ascii_lowercase().as_str() {
        "me" => "me",
        _ => "all",
    }
}

/// List pull requests for the repository at `toplevel`, using a 30s in-memory
/// TTL cache unless `refresh` is `true`.
pub async fn list_pull_requests(
    toplevel: &Path,
    state: &str,
    author: &str,
    page: u32,
    limit: u32,
    refresh: bool,
) -> PullRequestsPage {
    let state = normalize_pr_state(state);
    let author = normalize_pr_author(author);
    let page = page.max(1);
    let limit = if limit == 0 { 10 } else { limit.clamp(1, 30) };

    let key = CacheKey {
        toplevel: toplevel.to_path_buf(),
        state: state.to_string(),
        author: author.to_string(),
        page,
        limit,
    };

    if !refresh {
        if let Some(cached) = cache_get(&key, Instant::now()) {
            return cached;
        }
    }

    let result = fetch_pull_requests(toplevel, state, author, page, limit).await;
    cache_put(key, result.clone(), Instant::now());
    result
}

async fn fetch_pull_requests(
    toplevel: &Path,
    state: &str,
    author: &str,
    page: u32,
    limit: u32,
) -> PullRequestsPage {
    let remote = match git::origin_remote(toplevel).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            return PullRequestsPage::unsupported(
                "none",
                "No origin remote configured for this repository",
                None,
                page,
            );
        }
        Err(err) => {
            tracing::warn!("git origin remote lookup failed: {err}");
            return PullRequestsPage::unsupported(
                "remote",
                "Could not inspect repository remote",
                None,
                page,
            );
        }
    };

    match remote.host_kind.as_str() {
        "github" => fetch_github_prs(toplevel, &remote, state, author, page, limit).await,
        "gitea" => fetch_gitea_prs(toplevel, &remote, state, page, limit).await,
        other => PullRequestsPage::unsupported(
            other,
            "PR listing requires GitHub (gh CLI) or Gitea (tea CLI)",
            Some(remote.web_url),
            page,
        ),
    }
}

fn valid_repo_slug_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 128
        && !segment.starts_with('-')
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn extract_owner_repo(remote: &RemoteInfo) -> Option<(&str, &str)> {
    let mut parts = remote.repo_path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if valid_repo_slug_segment(owner) && valid_repo_slug_segment(repo) {
        Some((owner, repo))
    } else {
        None
    }
}

async fn fetch_github_prs(
    toplevel: &Path,
    remote: &RemoteInfo,
    state: &str,
    author: &str,
    page: u32,
    limit: u32,
) -> PullRequestsPage {
    let Some((owner, repo)) = extract_owner_repo(remote) else {
        return PullRequestsPage::unsupported(
            "github",
            "Could not parse GitHub owner/repo from origin remote",
            Some(remote.web_url.clone()),
            page,
        );
    };

    if author == "me" {
        let state_qualifier = match state {
            "open" => "+is:open",
            "closed" => "+is:closed",
            _ => "",
        };
        let endpoint = format!(
            "search/issues?q=is:pr+repo:{owner}/{repo}+author:@me{state_qualifier}&per_page={limit}&page={page}&sort=updated&order=desc"
        );
        match run_cli(toplevel, "gh", &["api", &endpoint], PR_CLI_TIMEOUT).await {
            Ok(stdout) => match parse_github_search_response(&stdout, page, limit) {
                Ok((items, has_more)) => PullRequestsPage {
                    host: "github".into(),
                    supported: true,
                    reason: None,
                    repo_url: Some(remote.web_url.clone()),
                    items,
                    page,
                    next_page: has_more.then_some(page + 1),
                    has_more,
                },
                Err(err) => {
                    tracing::warn!("failed to parse gh search response: {err}");
                    PullRequestsPage::unsupported(
                        "github",
                        "Failed to parse pull request list from GitHub CLI",
                        Some(remote.web_url.clone()),
                        page,
                    )
                }
            },
            Err(err) => github_cli_error_page(err, remote.web_url.clone(), page),
        }
    } else {
        let endpoint = format!(
            "repos/{owner}/{repo}/pulls?state={state}&per_page={limit}&page={page}&sort=updated&direction=desc"
        );
        match run_cli(toplevel, "gh", &["api", "-i", &endpoint], PR_CLI_TIMEOUT).await {
            Ok(stdout) => match parse_github_pulls_response(&stdout, page, limit) {
                Ok((items, has_more)) => PullRequestsPage {
                    host: "github".into(),
                    supported: true,
                    reason: None,
                    repo_url: Some(remote.web_url.clone()),
                    items,
                    page,
                    next_page: has_more.then_some(page + 1),
                    has_more,
                },
                Err(err) => {
                    tracing::warn!("failed to parse gh pulls response: {err}");
                    PullRequestsPage::unsupported(
                        "github",
                        "Failed to parse pull request list from GitHub CLI",
                        Some(remote.web_url.clone()),
                        page,
                    )
                }
            },
            Err(err) => github_cli_error_page(err, remote.web_url.clone(), page),
        }
    }
}

fn github_cli_error_page(err: CliError, repo_url: String, page: u32) -> PullRequestsPage {
    let reason = match err {
        CliError::NotInstalled => "GitHub CLI (gh) is not installed on this host",
        CliError::Timeout => "GitHub CLI (gh) timed out while fetching pull requests",
        CliError::Failed(detail) => {
            tracing::warn!("gh api failed: {detail}");
            let lower = detail.to_ascii_lowercase();
            if lower.contains("not logged into")
                || lower.contains("gh auth login")
                || lower.contains("authentication")
                || lower.contains("http 401")
                || lower.contains("401 unauthorized")
            {
                "GitHub CLI (gh) is not authenticated; run `gh auth login` on the host"
            } else {
                "GitHub CLI (gh) failed to list pull requests"
            }
        }
    };
    PullRequestsPage::unsupported("github", reason, Some(repo_url), page)
}

async fn fetch_gitea_prs(
    toplevel: &Path,
    remote: &RemoteInfo,
    state: &str,
    page: u32,
    limit: u32,
) -> PullRequestsPage {
    let Some((owner, repo)) = extract_owner_repo(remote) else {
        return PullRequestsPage::unsupported(
            "gitea",
            "PR listing requires GitHub (gh CLI) or Gitea (tea CLI)",
            Some(remote.web_url.clone()),
            page,
        );
    };
    let repo_slug = format!("{owner}/{repo}");
    let page_str = page.to_string();
    let limit_str = limit.to_string();
    match run_cli(
        toplevel,
        "tea",
        &[
            "pulls", "list", "--repo", &repo_slug, "--state", state, "--page", &page_str,
            "--limit", &limit_str, "--output", "json",
        ],
        PR_CLI_TIMEOUT,
    )
    .await
    {
        Ok(stdout) => match parse_gitea_pulls_response(&stdout, &remote.web_url, limit) {
            Ok((items, has_more)) => PullRequestsPage {
                host: "gitea".into(),
                supported: true,
                reason: None,
                repo_url: Some(remote.web_url.clone()),
                items,
                page,
                next_page: has_more.then_some(page + 1),
                has_more,
            },
            Err(err) => {
                tracing::warn!("failed to parse tea pulls response: {err}");
                PullRequestsPage::unsupported(
                    "gitea",
                    "PR listing requires GitHub (gh CLI) or Gitea (tea CLI)",
                    Some(remote.web_url.clone()),
                    page,
                )
            }
        },
        Err(err) => {
            if let CliError::Failed(detail) = &err {
                tracing::warn!("tea pulls list failed: {detail}");
            }
            PullRequestsPage::unsupported(
                "gitea",
                "PR listing requires GitHub (gh CLI) or Gitea (tea CLI)",
                Some(remote.web_url.clone()),
                page,
            )
        }
    }
}

/// Check out a pull request by number. On GitHub repositories, tries
/// `gh pr checkout <pr_number>` first and falls back to fetching
/// `pull/<n>/head:pr-<n>` via git when `gh` is unavailable.
pub async fn checkout_pr(toplevel: &Path, pr_number: u64) -> Result<String, GitError> {
    if pr_number == 0 {
        return Err(GitError::InvalidInput(
            "pr_number must be greater than 0".into(),
        ));
    }

    let remote = git::origin_remote(toplevel).await.ok().flatten();
    if remote
        .as_ref()
        .is_some_and(|info| info.host_kind == "github")
    {
        let num_str = pr_number.to_string();
        match run_cli(
            toplevel,
            "gh",
            &["pr", "checkout", &num_str],
            PR_CHECKOUT_TIMEOUT,
        )
        .await
        {
            Ok(_) => {
                if let Ok(branch) = git::current_branch(toplevel).await {
                    return Ok(branch);
                }
                return Ok(format!("pr-{pr_number}"));
            }
            Err(CliError::NotInstalled) => {
                // Fall through to pure git fetch fallback.
            }
            Err(CliError::Timeout) => return Err(GitError::Timeout),
            Err(CliError::Failed(detail)) => {
                tracing::warn!(
                    pr_number,
                    "gh pr checkout failed, falling back to git fetch: {detail}"
                );
            }
        }
    }

    git::checkout_pr_ref(toplevel, pr_number).await
}

// ---------------------------------------------------------------------------
// Pure HTTP / JSON parsers (tested without spawning external CLIs)
// ---------------------------------------------------------------------------

/// Split `gh api -i` output into `(headers, body)`. Handles multiple HTTP
/// header blocks (e.g., redirects) and both CRLF and LF line endings.
fn split_http_headers_and_body(raw: &str) -> (&str, &str) {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("HTTP/") {
        return ("", trimmed);
    }
    let mut rest = trimmed;
    let mut last_headers = "";
    loop {
        if let Some(pos) = rest.find("\r\n\r\n") {
            last_headers = &rest[..pos];
            rest = &rest[pos + 4..];
        } else if let Some(pos) = rest.find("\n\n") {
            last_headers = &rest[..pos];
            rest = &rest[pos + 2..];
        } else {
            return (last_headers, rest.trim());
        }
        if !rest.starts_with("HTTP/") {
            return (last_headers, rest.trim());
        }
    }
}

/// Inspect HTTP response headers for a `Link:` header containing `rel="next"`.
/// Returns `Some(true)` if `Link:` has `rel="next"`, `Some(false)` if `Link:`
/// is present without `rel="next"`, or `None` if no `Link:` header was found.
fn link_header_has_next(headers: &str) -> Option<bool> {
    let mut saw_link = false;
    for line in headers.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("link") {
            saw_link = true;
            for part in value.split(',') {
                if part.contains("rel=\"next\"") || part.contains("rel=next") {
                    return Some(true);
                }
            }
        }
    }
    saw_link.then_some(false)
}

fn parse_github_pulls_response(
    raw: &str,
    _page: u32,
    limit: u32,
) -> Result<(Vec<PullRequestItem>, bool), String> {
    let (headers, body) = split_http_headers_and_body(raw);
    let value: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let array = value
        .as_array()
        .ok_or_else(|| "expected JSON array from GitHub pulls endpoint".to_string())?;

    let mut items = Vec::with_capacity(array.len());
    for entry in array {
        if let Some(item) = parse_github_pr_object(entry) {
            items.push(item);
        }
    }

    let has_more = match link_header_has_next(headers) {
        Some(has_next) => has_next,
        None => items.len() == limit as usize,
    };

    Ok((items, has_more))
}

fn parse_github_pr_object(entry: &Value) -> Option<PullRequestItem> {
    let number = entry.get("number")?.as_u64()?;
    let title = entry
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let merged = entry.get("merged_at").is_some_and(|v| !v.is_null());
    let state = if merged {
        "merged".to_string()
    } else {
        match entry.get("state").and_then(Value::as_str).unwrap_or("open") {
            "closed" => "closed".to_string(),
            "merged" => "merged".to_string(),
            _ => "open".to_string(),
        }
    };
    let draft = entry.get("draft").and_then(Value::as_bool).unwrap_or(false);
    let url = entry
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let author = entry
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let head_branch = entry
        .get("head")
        .and_then(|h| h.get("ref"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let base_branch = entry
        .get("base")
        .and_then(|b| b.get("ref"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let updated_at = entry
        .get("updated_at")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    Some(PullRequestItem {
        number,
        title,
        state,
        draft,
        url,
        author,
        head_branch,
        base_branch,
        updated_at,
        checks_status: None,
    })
}

fn parse_github_search_response(
    raw: &str,
    page: u32,
    limit: u32,
) -> Result<(Vec<PullRequestItem>, bool), String> {
    let value: Value = serde_json::from_str(raw.trim()).map_err(|e| e.to_string())?;
    let total_count = value
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let array = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "expected items array in GitHub search response".to_string())?;

    let mut items = Vec::with_capacity(array.len());
    for entry in array {
        let Some(number) = entry.get("number").and_then(Value::as_u64) else {
            continue;
        };
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let merged = entry
            .get("pull_request")
            .and_then(|pr| pr.get("merged_at"))
            .is_some_and(|v| !v.is_null());
        let state = if merged {
            "merged".to_string()
        } else {
            match entry.get("state").and_then(Value::as_str).unwrap_or("open") {
                "closed" => "closed".to_string(),
                "merged" => "merged".to_string(),
                _ => "open".to_string(),
            }
        };
        let draft = entry.get("draft").and_then(Value::as_bool).unwrap_or(false);
        let url = entry
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let author = entry
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let head_branch = entry
            .get("head")
            .and_then(|h| h.get("ref"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let base_branch = entry
            .get("base")
            .and_then(|b| b.get("ref"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let updated_at = entry
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        items.push(PullRequestItem {
            number,
            title,
            state,
            draft,
            url,
            author,
            head_branch,
            base_branch,
            updated_at,
            checks_status: None,
        });
    }

    let seen = u64::from(page).saturating_mul(u64::from(limit));
    let has_more = !items.is_empty() && seen < total_count;
    Ok((items, has_more))
}

fn parse_gitea_pulls_response(
    raw: &str,
    repo_url: &str,
    limit: u32,
) -> Result<(Vec<PullRequestItem>, bool), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok((Vec::new(), false));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
    let array = value
        .as_array()
        .ok_or_else(|| "expected JSON array from tea pulls list".to_string())?;

    let mut items = Vec::with_capacity(array.len());
    for entry in array {
        let Some(number) = entry
            .get("index")
            .or_else(|| entry.get("number"))
            .and_then(Value::as_u64)
        else {
            continue;
        };
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let merged = entry
            .get("merged")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || entry.get("merged_at").is_some_and(|v| !v.is_null());
        let state = if merged {
            "merged".to_string()
        } else {
            match entry.get("state").and_then(Value::as_str).unwrap_or("open") {
                "closed" => "closed".to_string(),
                "merged" => "merged".to_string(),
                _ => "open".to_string(),
            }
        };
        let draft = entry.get("draft").and_then(Value::as_bool).unwrap_or(false);
        let url = entry
            .get("url")
            .or_else(|| entry.get("html_url"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{repo_url}/pulls/{number}"));
        let author = entry
            .get("author")
            .and_then(Value::as_str)
            .or_else(|| {
                entry
                    .get("user")
                    .and_then(|u| u.get("login"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("")
            .to_string();
        let head_branch = entry
            .get("head")
            .and_then(|h| h.as_str().or_else(|| h.get("ref").and_then(Value::as_str)))
            .unwrap_or("")
            .to_string();
        let base_branch = entry
            .get("base")
            .and_then(|b| b.as_str().or_else(|| b.get("ref").and_then(Value::as_str)))
            .unwrap_or("")
            .to_string();
        let updated_at = entry
            .get("updated")
            .or_else(|| entry.get("updated_at"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        items.push(PullRequestItem {
            number,
            title,
            state,
            draft,
            url,
            author,
            head_branch,
            base_branch,
            updated_at,
            checks_status: None,
        });
    }

    let has_more = items.len() == limit as usize;
    Ok((items, has_more))
}

// ---------------------------------------------------------------------------
// Bounded CLI execution
// ---------------------------------------------------------------------------

enum CliError {
    NotInstalled,
    Timeout,
    Failed(String),
}

async fn run_cli(
    cwd: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, CliError> {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    git::scrub_git_env(cmd.as_std_mut());

    let mut child = cmd.spawn().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => CliError::NotInstalled,
        _ => CliError::Failed(err.to_string()),
    })?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let read_stdout = async {
        let mut bytes = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let read = stdout.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            let retained = read.min(MAX_CLI_STDOUT_BYTES - bytes.len());
            bytes.extend_from_slice(&chunk[..retained]);
            if bytes.len() >= MAX_CLI_STDOUT_BYTES {
                let _ = child.start_kill();
                break;
            }
        }
        let status = child.wait().await;
        (bytes, status)
    };
    let read_stderr = async {
        let mut bytes = Vec::new();
        let mut chunk = vec![0u8; 4096];
        loop {
            let read = stderr.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            if bytes.len() < MAX_CLI_STDERR_BYTES {
                let retained = read.min(MAX_CLI_STDERR_BYTES - bytes.len());
                bytes.extend_from_slice(&chunk[..retained]);
            }
        }
        bytes
    };

    let result = tokio::time::timeout(timeout, async {
        let ((stdout, status), stderr) = tokio::join!(read_stdout, read_stderr);
        (stdout, stderr, status)
    })
    .await;

    let (stdout, stderr, status) = match result {
        Ok(value) => value,
        Err(_) => return Err(CliError::Timeout),
    };
    let status = status.map_err(|err| CliError::Failed(err.to_string()))?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(CliError::Failed(format!(
            "exit {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_header_detects_next_and_last_pages() {
        let with_next = concat!(
            "HTTP/2.0 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Link: <https://api.github.com/repositories/1/pulls?page=2>; rel=\"next\", ",
            "<https://api.github.com/repositories/1/pulls?page=5>; rel=\"last\"\r\n",
        );
        assert_eq!(link_header_has_next(with_next), Some(true));

        let last_page = concat!(
            "HTTP/2.0 200 OK\r\n",
            "Link: <https://api.github.com/repositories/1/pulls?page=4>; rel=\"prev\", ",
            "<https://api.github.com/repositories/1/pulls?page=1>; rel=\"first\"\r\n",
        );
        assert_eq!(link_header_has_next(last_page), Some(false));

        let no_link = "HTTP/2.0 200 OK\r\nContent-Type: application/json\r\n";
        assert_eq!(link_header_has_next(no_link), None);
    }

    #[test]
    fn parses_github_rest_pulls_with_link_header_and_merged_state() {
        let raw = concat!(
            "HTTP/2.0 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Link: <https://api.github.com/repos/o/r/pulls?page=2>; rel=\"next\"\r\n",
            "\r\n",
            r#"[
              {
                "number": 42,
                "title": "Add git workbench",
                "state": "open",
                "draft": true,
                "html_url": "https://github.com/o/r/pull/42",
                "user": { "login": "alice" },
                "head": { "ref": "feat/workbench" },
                "base": { "ref": "main" },
                "updated_at": "2026-11-27T10:00:00Z",
                "merged_at": null
              },
              {
                "number": 41,
                "title": "Merged PR",
                "state": "closed",
                "draft": false,
                "html_url": "https://github.com/o/r/pull/41",
                "user": { "login": "bob" },
                "head": { "ref": "fix/bug" },
                "base": { "ref": "main" },
                "updated_at": "2026-11-26T08:00:00Z",
                "merged_at": "2026-11-26T08:00:00Z"
              }
            ]"#
        );

        let (items, has_more) = parse_github_pulls_response(raw, 1, 10).unwrap();
        assert!(has_more);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].number, 42);
        assert_eq!(items[0].state, "open");
        assert!(items[0].draft);
        assert_eq!(items[0].author, "alice");
        assert_eq!(items[0].head_branch, "feat/workbench");
        assert_eq!(items[0].base_branch, "main");
        assert_eq!(items[1].number, 41);
        assert_eq!(items[1].state, "merged");
        assert!(!items[1].draft);

        // When Link header has no rel="next", has_more is false even if len == limit.
        let raw_last = concat!(
            "HTTP/2.0 200 OK\r\n",
            "Link: <https://api.github.com/repos/o/r/pulls?page=1>; rel=\"prev\"\r\n",
            "\r\n",
            r#"[{"number":1,"title":"T","state":"open","draft":false,"html_url":"u","user":{"login":"a"},"head":{"ref":"h"},"base":{"ref":"b"},"updated_at":"t","merged_at":null}]"#
        );
        let (items_last, has_more_last) = parse_github_pulls_response(raw_last, 2, 1).unwrap();
        assert_eq!(items_last.len(), 1);
        assert!(!has_more_last);
    }

    #[test]
    fn parses_github_search_issues_response_with_total_count_pagination() {
        let raw = r#"{
          "total_count": 25,
          "items": [
            {
              "number": 101,
              "title": "My PR",
              "state": "closed",
              "draft": false,
              "html_url": "https://github.com/o/r/pull/101",
              "user": { "login": "me-user" },
              "updated_at": "2026-11-27T11:00:00Z",
              "pull_request": { "merged_at": "2026-11-27T11:00:00Z" }
            }
          ]
        }"#;

        let (items, has_more_p1) = parse_github_search_response(raw, 1, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].number, 101);
        assert_eq!(items[0].state, "merged");
        assert_eq!(items[0].author, "me-user");
        assert!(has_more_p1);

        let (_, has_more_p3) = parse_github_search_response(raw, 3, 10).unwrap();
        assert!(!has_more_p3);
    }

    #[test]
    fn ttl_cache_serves_within_window_and_expires_after() {
        let key = CacheKey {
            toplevel: PathBuf::from("/tmp/cache-test-unique-repo"),
            state: "open".into(),
            author: "all".into(),
            page: 1,
            limit: 10,
        };
        let page = PullRequestsPage {
            host: "github".into(),
            supported: true,
            reason: None,
            repo_url: Some("https://github.com/o/r".into()),
            items: vec![],
            page: 1,
            next_page: None,
            has_more: false,
        };
        let t0 = Instant::now();
        cache_put(key.clone(), page.clone(), t0);

        assert_eq!(cache_get(&key, t0 + Duration::from_secs(5)), Some(page));
        assert_eq!(
            cache_get(&key, t0 + PR_CACHE_TTL + Duration::from_secs(1)),
            None
        );
    }

    #[tokio::test]
    async fn unsupported_remote_returns_graceful_fallback() {
        let root = std::env::temp_dir().join(format!(
            "muqun-prs-unsupported-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let status = std::process::Command::new("git")
            .args([
                "-C",
                root.to_str().unwrap(),
                "init",
                "--initial-branch",
                "main",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args([
                "-C",
                root.to_str().unwrap(),
                "remote",
                "add",
                "origin",
                "git@gitlab.com:acme/widget.git",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let page = list_pull_requests(&root, "open", "all", 1, 10, true).await;
        assert_eq!(page.host, "gitlab");
        assert!(!page.supported);
        assert!(page
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("GitHub (gh CLI) or Gitea (tea CLI)"));
        assert_eq!(
            page.repo_url.as_deref(),
            Some("https://gitlab.com/acme/widget")
        );
        assert!(page.items.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.next_page, None);

        std::fs::remove_dir_all(&root).ok();
    }
}
