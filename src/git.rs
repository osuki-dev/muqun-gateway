//! Read-only `git` for one pane's checkout, bounded in every direction.
//!
//! The diff viewer on the phone asks two questions -- "what changed in this
//! checkout?" and "show me one file's patch" -- and both are answered by
//! running `git` on the host rather than by reading the terminal, which is
//! where the pager, the colours and whatever the agent's shell is doing would
//! all get in the way. `docs/git-diff-viewer.md` in the app repository is the
//! design of record; this module is its Gateway half.
//!
//! Three rules, and every function here keeps all three:
//!
//! - **No client argument reaches git.** Each verb has a fixed argument list,
//!   the one client-supplied value is a path that has been validated and sits
//!   after a literal `--`, and the numbers are clamped before they are typed
//!   into an argument.
//! - **Bounded.** A timeout, a cap on stdout, `kill_on_drop`, and a cap on
//!   how many files a status lists, so a checkout the size of a monorepo costs
//!   the host one bounded process and the phone one bounded payload.
//! - **Read-only, and it stays out of the agent's way.** `--no-optional-locks`
//!   so a status never takes `index.lock` from the agent that is working in
//!   the same checkout, and nothing here writes.
//!
//! Environment: the `TMUX` and `HERDR_*` variables a manually launched gateway
//! carries are removed, as `backend_startup` removes them, and so are
//! `GIT_DIR`/`GIT_WORK_TREE`, which would otherwise redirect every command to
//! whatever repository the gateway happened to be started from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

/// One git process gets this long. A status on a large checkout is tens of
/// milliseconds; a diff of one file is less. Five seconds is "the disk is
/// asleep", not "the repository is big".
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Stdout is read up to here and the process is then killed. A single file's
/// patch that is bigger than this is not read on a phone.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// A status lists at most this many files, and says so when it stopped.
pub const MAX_STATUS_FILES: usize = 2000;
/// The most patch lines one answer carries; a longer patch is paged.
pub const FILE_PATCH_MAX_LINES: usize = 4000;
pub const DEFAULT_CONTEXT_LINES: u32 = 3;
pub const MAX_CONTEXT_LINES: u32 = 25;
/// An untracked file's line count comes from reading it, so the read is
/// bounded; a larger file answers "unknown" rather than being read.
const MAX_COUNTED_UNTRACKED_BYTES: u64 = 1024 * 1024;
/// How much of stderr is kept for the log. Never forwarded to a client.
const MAX_STDERR_BYTES: usize = 4096;

#[derive(Debug)]
pub enum GitError {
    /// `git` is not on the gateway's PATH.
    NotInstalled,
    Timeout,
    /// git exited non-zero; the text is for the gateway's log only.
    Failed(String),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => formatter.write_str("git is not installed"),
            Self::Timeout => formatter.write_str("git timed out"),
            Self::Failed(detail) => write!(formatter, "git failed: {detail}"),
        }
    }
}

/// The branch line of a checkout, plus how many files a status listed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoSummary {
    pub toplevel: PathBuf,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<i64>,
    pub behind: Option<i64>,
    pub detached: bool,
    /// Abbreviated to what a phone shows; `None` on an unborn branch.
    pub head: Option<String>,
    pub changed_files: usize,
}

impl RepoSummary {
    pub fn to_json(&self) -> Value {
        json!({
            "toplevel": self.toplevel.to_string_lossy(),
            "branch": self.branch,
            "upstream": self.upstream,
            "ahead": self.ahead,
            "behind": self.behind,
            "detached": self.detached,
            "head": self.head,
            "changed_files": self.changed_files,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
    TypeChanged,
}

impl ChangeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
            Self::TypeChanged => "type_changed",
        }
    }
}

/// One changed file, as the list on the phone shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub staged: bool,
    pub unstaged: bool,
    pub binary: bool,
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

impl FileChange {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "old_path": self.old_path,
            "status": self.status.as_str(),
            "staged": self.staged,
            "unstaged": self.unstaged,
            "binary": self.binary,
            "added": self.added,
            "removed": self.removed,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub summary: RepoSummary,
    pub files: Vec<FileChange>,
    /// The list stopped at [`MAX_STATUS_FILES`]; `summary.changed_files` is
    /// still the full count.
    pub truncated: bool,
}

/// Which side of the index a patch compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchSide {
    /// Working tree against `HEAD`: staged and unstaged together, which is
    /// what "what did the agent change" means.
    WorkingTreeVsHead,
    /// Index against `HEAD`.
    Staged,
    /// Working tree against the index.
    Unstaged,
}

/// One page of one file's unified patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: String,
    pub binary: bool,
    pub from: usize,
    pub end: usize,
    pub total_lines: usize,
    pub truncated: bool,
    pub patch: String,
}

impl FilePatch {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "binary": self.binary,
            "from": self.from,
            "end": self.end,
            "total_lines": self.total_lines,
            "truncated": self.truncated,
            "patch": self.patch,
        })
    }
}

/// The checkout a directory belongs to, or `None` when it belongs to none.
///
/// `rev-parse` rather than a `.git` test because a pane's cwd is usually a
/// subdirectory of the checkout, not its top.
pub async fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let output = run(cwd, &["rev-parse", "--show-toplevel"], &[0])
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    std::fs::canonicalize(line).ok()
}

/// What changed in the checkout, with per-file line totals.
///
/// Two processes: `status --porcelain=v2` for the branch line and the entries,
/// `diff --numstat HEAD` for the totals. Untracked files are not in a diff
/// against `HEAD`, so their line count is read from the file itself, bounded.
pub async fn status(toplevel: &Path) -> Result<Status, GitError> {
    let (summary, entries) = porcelain(toplevel).await?;

    // `HEAD` does not exist on an unborn branch; the totals are simply unknown
    // then, which the phone shows as no number rather than as an error.
    let numstat = match run(toplevel, &["diff", "--numstat", "-M", "-z", "HEAD"], &[0]).await {
        Ok(output) => parse_numstat_z(&output.stdout),
        Err(GitError::Failed(_)) => HashMap::new(),
        Err(err) => return Err(err),
    };

    let truncated = entries.len() > MAX_STATUS_FILES;
    let mut files = Vec::with_capacity(entries.len().min(MAX_STATUS_FILES));
    for entry in entries.into_iter().take(MAX_STATUS_FILES) {
        let counts = numstat.get(&entry.path).copied();
        let (added, removed, binary) = match (entry.status, counts) {
            (_, Some(Counts::Binary)) => (None, None, true),
            (_, Some(Counts::Lines { added, removed })) => (Some(added), Some(removed), false),
            (ChangeStatus::Untracked, None) => {
                let (lines, binary) = count_untracked_lines(&toplevel.join(&entry.path));
                (lines, Some(0), binary)
            }
            (_, None) => (None, None, false),
        };
        files.push(FileChange {
            path: entry.path,
            old_path: entry.old_path,
            status: entry.status,
            staged: entry.staged,
            unstaged: entry.unstaged,
            binary,
            added,
            removed,
        });
    }

    Ok(Status {
        summary,
        files,
        truncated,
    })
}

/// The branch line and the changed-file count, and nothing per file: what
/// the pane context and the badge need, at the cost of one process.
pub async fn summary(toplevel: &Path) -> Result<RepoSummary, GitError> {
    Ok(porcelain(toplevel).await?.0)
}

async fn porcelain(toplevel: &Path) -> Result<(RepoSummary, Vec<Entry>), GitError> {
    let output = run(
        toplevel,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=normal",
            "--renames",
        ],
        &[0],
    )
    .await?;
    let (mut summary, entries) = parse_porcelain_v2(&output.stdout);
    summary.toplevel = toplevel.to_path_buf();
    summary.changed_files = entries.len();
    Ok((summary, entries))
}

/// One file's unified patch, one page of it.
///
/// `path` is the client's one input and is validated before anything else: a
/// relative path with no `..`, no leading `-`, no NUL, that git receives after
/// a literal `--`. A tracked path is diffed by git within the checkout, which
/// cannot reach outside it. An untracked path is a real file on disk, so it is
/// additionally required to be a regular file (not a symlink) whose canonical
/// form is inside the checkout, and it is rendered as an all-additions patch
/// against `/dev/null`.
pub async fn file_patch(
    toplevel: &Path,
    path: &str,
    old_path: Option<&str>,
    side: PatchSide,
    context: u32,
    from: usize,
    lines: usize,
) -> Result<Option<FilePatch>, GitError> {
    let Some(relative) = validate_relative_path(path) else {
        return Ok(None);
    };
    let relative_str = relative.to_string_lossy().into_owned();
    // A rename is only a rename when git can see both sides: with the new
    // path alone as the pathspec, the file is a brand-new one. The old path
    // is validated exactly as the new one and rides after the same `--`.
    let old_relative = match old_path {
        Some(old) => match validate_relative_path(old) {
            Some(old) => Some(old.to_string_lossy().into_owned()),
            None => return Ok(None),
        },
        None => None,
    };
    let context = context.min(MAX_CONTEXT_LINES);
    let unified = format!("-U{context}");
    let lines = lines.clamp(1, FILE_PATCH_MAX_LINES);

    let mut args: Vec<&str> = vec![
        "diff",
        "-M",
        &unified,
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
    ];
    match side {
        PatchSide::WorkingTreeVsHead => args.push("HEAD"),
        PatchSide::Staged => args.push("--cached"),
        PatchSide::Unstaged => {}
    }
    args.push("--");
    args.push(&relative_str);
    if let Some(old) = &old_relative {
        args.push(old);
    }

    let mut text = match run(toplevel, &args, &[0, 1]).await {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        // An unborn branch has no HEAD; the file is then new by definition.
        Err(GitError::Failed(_)) if side == PatchSide::WorkingTreeVsHead => String::new(),
        Err(err) => return Err(err),
    };

    if text.is_empty() && side == PatchSide::WorkingTreeVsHead {
        let tracked = run(toplevel, &["ls-files", "-z", "--", &relative_str], &[0]).await?;
        if tracked.stdout.is_empty() {
            let Some(_) = untracked_file_inside(toplevel, &relative) else {
                return Ok(None);
            };
            let output = run(
                toplevel,
                &[
                    "diff",
                    &unified,
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-index",
                    "--",
                    "/dev/null",
                    &relative_str,
                ],
                &[0, 1],
            )
            .await?;
            text = String::from_utf8_lossy(&output.stdout).into_owned();
        }
    }

    let binary = is_binary_patch(&text);
    let (from, end, total_lines, truncated, page) = page_lines(&text, from, lines);
    Ok(Some(FilePatch {
        path: relative_str,
        binary,
        from,
        end,
        total_lines,
        truncated,
        patch: page,
    }))
}

// ---------------------------------------------------------------------------
// The process
// ---------------------------------------------------------------------------

struct Output {
    stdout: Vec<u8>,
}

fn command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--no-pager")
        .arg("-C")
        .arg(cwd)
        // A non-ASCII path arrives as UTF-8 rather than octal escapes; the
        // prefixes the phone's parser keys off are always `a/` and `b/`; and
        // no `color.ui` in the user's config reaches the wire.
        .args([
            "-c",
            "core.quotepath=false",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "color.ui=never",
        ])
        .arg("--no-optional-locks")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .kill_on_drop(true);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "TMUX",
        "HERDR_SESSION",
        "HERDR_SOCKET_PATH",
        "HERDR_CLIENT_SOCKET_PATH",
        "HERDR_PANE_ID",
        "HERDR_TAB_ID",
        "HERDR_WORKSPACE_ID",
        "HERDR_ENV",
    ] {
        cmd.env_remove(name);
    }
    cmd
}

/// Run one git command with the fixed prefix, a timeout and a stdout cap.
///
/// `ok_codes` is which exit codes count as success: `diff --no-index` exits 1
/// when the files differ, which is the answer being asked for.
async fn run(cwd: &Path, args: &[&str], ok_codes: &[i32]) -> Result<Output, GitError> {
    let mut cmd = command(cwd);
    cmd.args(args);
    let mut child = cmd.spawn().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => GitError::NotInstalled,
        _ => GitError::Failed(err.to_string()),
    })?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let read_stdout = async {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let read = stdout.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() >= MAX_OUTPUT_BYTES {
                break;
            }
        }
        bytes
    };
    let read_stderr = async {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stderr.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            if bytes.len() < MAX_STDERR_BYTES {
                bytes.extend_from_slice(&chunk[..read]);
            }
        }
        bytes
    };

    let result = tokio::time::timeout(GIT_TIMEOUT, async {
        let (stdout, stderr) = tokio::join!(read_stdout, read_stderr);
        let capped = stdout.len() >= MAX_OUTPUT_BYTES;
        if capped {
            let _ = child.kill().await;
        }
        let status = child.wait().await;
        (stdout, stderr, capped, status)
    })
    .await;

    let (mut stdout, stderr, capped, status) = match result {
        Ok(value) => value,
        // Dropping the future above killed the child (`kill_on_drop`).
        Err(_) => return Err(GitError::Timeout),
    };
    if capped {
        // Cut on a line so the caller never sees half a line.
        if let Some(cut) = stdout.iter().rposition(|byte| *byte == b'\n') {
            stdout.truncate(cut + 1);
        }
        return Ok(Output { stdout });
    }
    let status = status.map_err(|err| GitError::Failed(err.to_string()))?;
    let code = status.code().unwrap_or(-1);
    if !ok_codes.contains(&code) {
        return Err(GitError::Failed(format!(
            "exit {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    Ok(Output { stdout })
}

// ---------------------------------------------------------------------------
// Pure parsing, tested without a process
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    path: String,
    old_path: Option<String>,
    status: ChangeStatus,
    staged: bool,
    unstaged: bool,
}

/// `git status --porcelain=v2 -z --branch`.
///
/// Records are NUL-terminated; a rename record (`2 …`) is followed by one more
/// NUL-terminated field holding the original path. Header lines start with
/// `# branch.`; nothing else in the output starts with `#`, because a path
/// starting with `#` is preceded by its record type.
fn parse_porcelain_v2(bytes: &[u8]) -> (RepoSummary, Vec<Entry>) {
    let mut summary = RepoSummary::default();
    let mut entries = Vec::new();
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix("# ") {
            parse_branch_header(header, &mut summary);
            continue;
        }
        let mut parts = record.splitn(2, ' ');
        let kind = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        match kind {
            "1" => {
                // XY sub mH mI mW hH hI path -- eight pieces, and the path
                // is the remainder so a space in it survives.
                let mut columns = rest.splitn(8, ' ');
                let xy = columns.next().unwrap_or("..");
                let path = columns.nth(6).unwrap_or("").to_owned();
                let (staged, unstaged) = staged_unstaged(xy);
                entries.push(Entry {
                    path,
                    old_path: None,
                    status: ordinary_status(xy),
                    staged,
                    unstaged,
                });
            }
            "2" => {
                // XY sub mH mI mW hH hI Xscore path, then the original path
                let mut columns = rest.splitn(9, ' ');
                let xy = columns.next().unwrap_or("..");
                let score = columns.nth(6).unwrap_or("");
                let path = columns.next().unwrap_or("").to_owned();
                let old_path = fields.next().filter(|value| !value.is_empty());
                let (staged, unstaged) = staged_unstaged(xy);
                let status = if score.starts_with('C') {
                    ChangeStatus::Copied
                } else {
                    ChangeStatus::Renamed
                };
                entries.push(Entry {
                    path,
                    old_path,
                    status,
                    staged,
                    unstaged,
                });
            }
            "u" => {
                // XY sub m1 m2 m3 mW h1 h2 h3 path -- ten pieces
                let path = rest.splitn(10, ' ').nth(9).unwrap_or("").to_owned();
                entries.push(Entry {
                    path,
                    old_path: None,
                    status: ChangeStatus::Conflicted,
                    staged: true,
                    unstaged: true,
                });
            }
            "?" => entries.push(Entry {
                path: rest.to_owned(),
                old_path: None,
                status: ChangeStatus::Untracked,
                staged: false,
                unstaged: true,
            }),
            // `!` is an ignored file; never listed.
            _ => {}
        }
    }
    (summary, entries)
}

fn parse_branch_header(header: &str, summary: &mut RepoSummary) {
    let mut parts = header.splitn(2, ' ');
    let key = parts.next().unwrap_or("");
    let value = parts.next().unwrap_or("").trim();
    match key {
        "branch.oid" => {
            summary.head = (value != "(initial)").then(|| value.chars().take(7).collect());
        }
        "branch.head" => {
            if value == "(detached)" {
                summary.detached = true;
                summary.branch = None;
            } else {
                summary.branch = Some(value.to_owned());
            }
        }
        "branch.upstream" => summary.upstream = Some(value.to_owned()),
        "branch.ab" => {
            for token in value.split(' ') {
                if let Some(ahead) = token.strip_prefix('+') {
                    summary.ahead = ahead.parse().ok();
                } else if let Some(behind) = token.strip_prefix('-') {
                    summary.behind = behind.parse().ok();
                }
            }
        }
        _ => {}
    }
}

fn staged_unstaged(xy: &str) -> (bool, bool) {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    (x != '.', y != '.')
}

fn ordinary_status(xy: &str) -> ChangeStatus {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    let code = if x != '.' { x } else { y };
    match code {
        'A' => ChangeStatus::Added,
        'D' => ChangeStatus::Deleted,
        'T' => ChangeStatus::TypeChanged,
        'R' => ChangeStatus::Renamed,
        'C' => ChangeStatus::Copied,
        _ => ChangeStatus::Modified,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counts {
    Lines { added: u64, removed: u64 },
    Binary,
}

/// `git diff --numstat -M -z`.
///
/// `added TAB removed TAB path NUL`, except that a rename is
/// `added TAB removed TAB NUL old NUL new NUL`. Binary files count as `-`.
fn parse_numstat_z(bytes: &[u8]) -> HashMap<String, Counts> {
    let mut counts = HashMap::new();
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut columns = record.splitn(3, '\t');
        let added = columns.next().unwrap_or("");
        let removed = columns.next().unwrap_or("");
        let mut path = columns.next().unwrap_or("").to_owned();
        if path.is_empty() {
            // A rename: skip the old path, keep the new one.
            let _old = fields.next();
            path = fields.next().unwrap_or_default();
        }
        if path.is_empty() {
            continue;
        }
        let value = if added == "-" || removed == "-" {
            Counts::Binary
        } else {
            Counts::Lines {
                added: added.parse().unwrap_or(0),
                removed: removed.parse().unwrap_or(0),
            }
        };
        counts.insert(path, value);
    }
    counts
}

/// The one client-supplied value, checked before it goes anywhere near an
/// argument list. Relative, made only of normal components, not starting
/// with `-`, no NUL, and not empty.
pub fn validate_relative_path(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.starts_with('-') || path.contains('\0') {
        return None;
    }
    if Path::new(path).is_absolute() {
        return None;
    }
    // Component by component on the string, not through `Path::components`,
    // which quietly drops a `.` in the middle: `a/./b` must be refused as
    // written, not normalized into something acceptable.
    let mut normal = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        normal.push(part);
    }
    (!normal.as_os_str().is_empty()).then_some(normal)
}

/// An untracked path is a real file the gateway is about to hand to
/// `diff --no-index`, so it has to be a regular file (a symlink is refused:
/// `--no-index` would read through it) whose canonical form is inside the
/// checkout.
fn untracked_file_inside(toplevel: &Path, relative: &Path) -> Option<PathBuf> {
    let full = toplevel.join(relative);
    let metadata = std::fs::symlink_metadata(&full).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let canonical = std::fs::canonicalize(&full).ok()?;
    let root = std::fs::canonicalize(toplevel).ok()?;
    (canonical != root && canonical.starts_with(&root)).then_some(canonical)
}

fn count_untracked_lines(path: &Path) -> (Option<u64>, bool) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return (None, false);
    };
    if !metadata.is_file() || metadata.len() > MAX_COUNTED_UNTRACKED_BYTES {
        return (None, false);
    }
    let Ok(bytes) = std::fs::read(path) else {
        return (None, false);
    };
    if bytes.iter().take(8000).any(|byte| *byte == 0) {
        return (None, true);
    }
    let mut lines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.last().is_some_and(|byte| *byte != b'\n') {
        lines += 1;
    }
    (Some(lines), false)
}

fn is_binary_patch(text: &str) -> bool {
    !text.contains("\n@@") && text.lines().any(|line| line.starts_with("Binary files "))
}

/// One page of a patch, cut on a hunk boundary when there is one to cut on.
///
/// `from` is a line offset into the whole patch. The page ends at
/// `from + lines`, moved back to the nearest hunk (`@@`) or file (`diff
/// --git`) boundary so the next page starts on one and parses on its own --
/// but only to a boundary past the middle of the page. A hunk longer than a
/// page (a file where every third line changed is one hunk) would otherwise
/// leave the first page holding four header lines and nothing to read; such
/// a hunk is cut raw, and the reader continues it from the line counters of
/// the page before. Returns `(from, end, total_lines, truncated, text)`.
fn page_lines(text: &str, from: usize, lines: usize) -> (usize, usize, usize, bool, String) {
    let all: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        text.strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .collect()
    };
    let total = all.len();
    let from = from.min(total);
    let mut end = (from + lines).min(total);
    if end < total {
        // Strictly past the middle: a boundary sitting exactly there would
        // still hand back half a page.
        let floor = from + lines / 2 + 1;
        let boundary = (floor.max(from + 1)..end)
            .rev()
            .find(|index| all[*index].starts_with("@@") || all[*index].starts_with("diff --git "));
        if let Some(boundary) = boundary {
            end = boundary;
        }
    }
    let mut page = all[from..end].join("\n");
    if end > from {
        page.push('\n');
    }
    (from, end, total, end < total, page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2_reads_the_branch_and_every_kind_of_entry() {
        let bytes = concat!(
            "# branch.oid 70c8c85abcdef\0",
            "# branch.head feat/git-diff\0",
            "# branch.upstream origin/main\0",
            "# branch.ab +2 -1\0",
            "1 .M N... 100644 100644 100644 abc def src/lib/gateway-client.ts\0",
            "1 A. N... 000000 100644 100644 000 def src/lib/new.ts\0",
            "1 .D N... 100644 100644 000000 abc 000 old.txt\0",
            "1 .M N... 100644 100644 100644 abc def docs/with space.md\0",
            "2 R. N... 100644 100644 100644 abc abc R100 src/renamed.ts\0src/original.ts\0",
            "u UU N... 100644 100644 100644 100644 a b c conflict.txt\0",
            "? notes/todo with space.md\0",
            "? weird\nname.txt\0",
            "! ignored.log\0",
        )
        .as_bytes();
        let (summary, entries) = parse_porcelain_v2(bytes);
        assert_eq!(summary.branch.as_deref(), Some("feat/git-diff"));
        assert_eq!(summary.upstream.as_deref(), Some("origin/main"));
        assert_eq!(summary.ahead, Some(2));
        assert_eq!(summary.behind, Some(1));
        assert_eq!(summary.head.as_deref(), Some("70c8c85"));
        assert!(!summary.detached);

        let paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "src/lib/gateway-client.ts",
                "src/lib/new.ts",
                "old.txt",
                "docs/with space.md",
                "src/renamed.ts",
                "conflict.txt",
                "notes/todo with space.md",
                "weird\nname.txt",
            ]
        );
        assert_eq!(entries[0].status, ChangeStatus::Modified);
        assert!(!entries[0].staged && entries[0].unstaged);
        assert_eq!(entries[1].status, ChangeStatus::Added);
        assert!(entries[1].staged && !entries[1].unstaged);
        assert_eq!(entries[2].status, ChangeStatus::Deleted);
        assert_eq!(entries[3].status, ChangeStatus::Modified);
        assert_eq!(entries[4].status, ChangeStatus::Renamed);
        assert_eq!(entries[4].old_path.as_deref(), Some("src/original.ts"));
        assert_eq!(entries[5].status, ChangeStatus::Conflicted);
        assert_eq!(entries[6].status, ChangeStatus::Untracked);
        // `-z` is what keeps a newline in a name from forging a record.
        assert_eq!(entries[7].path, "weird\nname.txt");
    }

    #[test]
    fn porcelain_v2_knows_a_detached_head_and_an_unborn_branch() {
        let (detached, _) =
            parse_porcelain_v2("# branch.oid abcdef0123\0# branch.head (detached)\0".as_bytes());
        assert!(detached.detached);
        assert_eq!(detached.branch, None);
        assert_eq!(detached.head.as_deref(), Some("abcdef0"));

        let (unborn, _) =
            parse_porcelain_v2("# branch.oid (initial)\0# branch.head main\0".as_bytes());
        assert_eq!(unborn.head, None);
        assert_eq!(unborn.branch.as_deref(), Some("main"));
    }

    #[test]
    fn numstat_z_reads_totals_renames_and_binaries() {
        let bytes =
            "41\t6\tsrc/a.ts\0-\t-\tassets/icon.png\x003\t0\t\0src/old.ts\0src/new.ts\0".as_bytes();
        let counts = parse_numstat_z(bytes);
        assert_eq!(
            counts.get("src/a.ts"),
            Some(&Counts::Lines {
                added: 41,
                removed: 6
            })
        );
        assert_eq!(counts.get("assets/icon.png"), Some(&Counts::Binary));
        assert_eq!(
            counts.get("src/new.ts"),
            Some(&Counts::Lines {
                added: 3,
                removed: 0
            })
        );
        assert!(!counts.contains_key("src/old.ts"));
    }

    #[test]
    fn a_path_is_relative_normal_and_never_an_argument() {
        for good in [
            "src/a.ts",
            "dir with space/x.md",
            "a/b/c",
            "weird\nname.txt",
            "src/.env",
        ] {
            assert!(
                validate_relative_path(good).is_some(),
                "{good:?} should pass"
            );
        }
        for bad in [
            "",
            "-x",
            "--cached",
            "../a",
            "a/../b",
            "/etc/passwd",
            "a\0b",
            ".",
            "./a",
            "a/./b",
        ] {
            assert!(validate_relative_path(bad).is_none(), "{bad:?} should fail");
        }
    }

    fn sample_patch() -> String {
        let mut text = String::from(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -10,3 +10,3 @@\n x\n-y\n+Y\n z\n",
        );
        text.push_str("@@ -20,2 +20,2 @@\n p\n-q\n+Q\n");
        text
    }

    #[test]
    fn a_page_ends_on_a_hunk_boundary_and_the_next_starts_on_one() {
        let text = sample_patch();
        // Page of 10 from 0: the cut at 10 backs up to the `@@` at 8, which is
        // past the middle of the page.
        let (from, end, total, truncated, page) = page_lines(&text, 0, 10);
        assert_eq!((from, end, total, truncated), (0, 8, 17, true));
        assert!(page.ends_with(" c\n"));

        let (from, end, _, truncated, page) = page_lines(&text, end, 7);
        assert_eq!((from, end, truncated), (8, 13, true));
        assert!(page.starts_with("@@ -10,3"));

        let (from, end, _, truncated, page) = page_lines(&text, end, 100);
        assert_eq!((from, end, truncated), (13, 17, false));
        assert!(page.starts_with("@@ -20,2"));
        assert!(page.ends_with("+Q\n"));
    }

    #[test]
    fn a_boundary_before_the_middle_of_the_page_is_not_worth_cutting_to() {
        // Header, then one hunk far longer than the page: the `@@` at line 3
        // is before the middle of a 10-line page, so the page is cut raw at 10
        // rather than delivering three header lines.
        let text = sample_patch();
        let (from, end, _, truncated, page) = page_lines(&text, 0, 6);
        assert_eq!((from, end, truncated), (0, 6, true));
        assert_eq!(page.lines().count(), 6);
    }

    #[test]
    fn a_hunk_longer_than_a_page_is_cut_raw_rather_than_never_delivered() {
        let mut text = String::from("@@ -1,50 +1,50 @@\n");
        for index in 0..50 {
            text.push_str(&format!(" line {index}\n"));
        }
        let (_, end, total, truncated, page) = page_lines(&text, 0, 10);
        assert_eq!((end, total, truncated), (10, 51, true));
        assert_eq!(page.lines().count(), 10);
        let (from, end, _, truncated, _) = page_lines(&text, 10, 100);
        assert_eq!((from, end, truncated), (10, 51, false));
    }

    #[test]
    fn an_empty_patch_pages_to_nothing_and_from_past_the_end_is_clamped() {
        assert_eq!(page_lines("", 0, 10), (0, 0, 0, false, String::new()));
        let text = sample_patch();
        let (from, end, total, truncated, page) = page_lines(&text, 999, 10);
        assert_eq!((from, end, total, truncated), (17, 17, 17, false));
        assert_eq!(page, "");
    }

    #[test]
    fn binary_is_the_marker_without_hunks() {
        assert!(is_binary_patch(
            "diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n"
        ));
        assert!(!is_binary_patch(&sample_patch()));
    }

    // ----- with a real repository -------------------------------------------

    fn git_ok(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn temp_repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "muqun-gateway-git-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_ok(&repo, &["init", "--initial-branch", "main"]);
        git_ok(&repo, &["config", "user.email", "test@example.com"]);
        git_ok(&repo, &["config", "user.name", "Test"]);
        git_ok(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("README.md"), "hello\nworld\n").unwrap();
        std::fs::write(
            repo.join("src/a.ts"),
            "const a = 1;\nconst b = 2;\nconst c = 3;\n",
        )
        .unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-q", "-m", "init"]);
        repo
    }

    #[tokio::test]
    async fn toplevel_is_found_from_a_subdirectory_and_not_outside_a_checkout() {
        let repo = temp_repo("toplevel");
        let found = toplevel(&repo.join("src")).await.unwrap();
        assert_eq!(found, std::fs::canonicalize(&repo).unwrap());
        let outside = std::env::temp_dir();
        // The system temp dir is not a checkout on a CI runner or a Mac.
        if !outside.join(".git").exists() {
            assert_eq!(toplevel(&outside).await, None);
        }
        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn status_lists_the_working_tree_against_head_with_totals() {
        let repo = temp_repo("status");
        std::fs::write(
            repo.join("src/a.ts"),
            "const a = 1;\nconst B = 2;\nconst c = 3;\nconst d = 4;\n",
        )
        .unwrap();
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("notes.md"), "one\ntwo\nthree").unwrap();
        std::fs::write(repo.join("src/staged.ts"), "export {};\n").unwrap();
        git_ok(&repo, &["add", "src/staged.ts"]);

        let status = status(&repo).await.unwrap();
        assert_eq!(status.summary.branch.as_deref(), Some("main"));
        assert!(status.summary.head.is_some());
        assert_eq!(status.summary.changed_files, 4);
        assert!(!status.truncated);

        let by_path: HashMap<&str, &FileChange> = status
            .files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        let modified = by_path["src/a.ts"];
        assert_eq!(modified.status, ChangeStatus::Modified);
        assert_eq!((modified.added, modified.removed), (Some(2), Some(1)));
        assert!(modified.unstaged && !modified.staged);

        let deleted = by_path["gone.txt"];
        assert_eq!(deleted.status, ChangeStatus::Deleted);
        assert_eq!((deleted.added, deleted.removed), (Some(0), Some(1)));

        let untracked = by_path["notes.md"];
        assert_eq!(untracked.status, ChangeStatus::Untracked);
        // Read from the file: three lines, the last without a newline.
        assert_eq!((untracked.added, untracked.removed), (Some(3), Some(0)));

        let staged = by_path["src/staged.ts"];
        assert_eq!(staged.status, ChangeStatus::Added);
        assert!(staged.staged && !staged.unstaged);
        assert_eq!((staged.added, staged.removed), (Some(1), Some(0)));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_file_patch_is_unified_paged_and_untracked_files_are_all_additions() {
        let repo = temp_repo("patch");
        std::fs::write(
            repo.join("src/a.ts"),
            "const a = 1;\nconst B = 2;\nconst c = 3;\n",
        )
        .unwrap();
        std::fs::write(repo.join("notes.md"), "one\ntwo\n").unwrap();

        let patch = file_patch(
            &repo,
            "src/a.ts",
            None,
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(patch
            .patch
            .starts_with("diff --git a/src/a.ts b/src/a.ts\n"));
        assert!(patch.patch.contains("\n-const b = 2;\n+const B = 2;\n"));
        assert!(!patch.binary);
        assert!(!patch.truncated);
        assert_eq!(patch.end, patch.total_lines);

        let untracked = file_patch(
            &repo,
            "notes.md",
            None,
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(untracked.patch.contains("--- /dev/null\n"));
        assert!(untracked.patch.contains("\n+one\n+two\n"));

        // Paged: two lines a page, every page starts where the last one ended.
        let first = file_patch(
            &repo,
            "src/a.ts",
            None,
            PatchSide::WorkingTreeVsHead,
            0,
            0,
            2,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(first.truncated);
        let second = file_patch(
            &repo,
            "src/a.ts",
            None,
            PatchSide::WorkingTreeVsHead,
            0,
            first.end,
            4000,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.from, first.end);
        assert!(!second.truncated);
        assert_eq!(first.total_lines, second.total_lines);

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_patch_refuses_paths_that_are_arguments_escapes_or_symlinks() {
        let repo = temp_repo("fence");
        let outside = repo.parent().unwrap().join("secret.txt");
        std::fs::write(&outside, "secret\n").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link.txt")).unwrap();

        for bad in ["--cached", "-U0", "../secret.txt", "/etc/passwd", ""] {
            let answer = file_patch(&repo, bad, None, PatchSide::WorkingTreeVsHead, 3, 0, 4000)
                .await
                .unwrap();
            assert!(answer.is_none(), "{bad:?} should be refused");
        }
        // An untracked symlink is refused outright; `--no-index` would read
        // through it.
        let link = file_patch(
            &repo,
            "link.txt",
            None,
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap();
        assert!(link.is_none());
        // A path that names nothing is an empty patch, not an error.
        let missing = file_patch(
            &repo,
            "nope.txt",
            None,
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap();
        assert!(missing.is_none());

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_rename_is_a_rename_only_when_both_paths_are_named() {
        let repo = temp_repo("rename");
        git_ok(&repo, &["mv", "README.md", "docs.md"]);
        std::fs::write(repo.join("docs.md"), "hello\nworld\nmore\n").unwrap();

        let alone = file_patch(
            &repo,
            "docs.md",
            None,
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(alone.patch.contains("new file mode"));

        let both = file_patch(
            &repo,
            "docs.md",
            Some("README.md"),
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(both
            .patch
            .contains("rename from README.md\nrename to docs.md\n"));
        assert!(both.patch.contains("\n+more\n"));

        // The old path is validated like the new one.
        let bad = file_patch(
            &repo,
            "docs.md",
            Some("../README.md"),
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap();
        assert!(bad.is_none());

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_binary_change_is_reported_without_hunks() {
        let repo = temp_repo("binary");
        std::fs::write(repo.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        git_ok(&repo, &["add", "blob.bin"]);
        git_ok(&repo, &["commit", "-q", "-m", "blob"]);
        std::fs::write(repo.join("blob.bin"), [0u8, 9, 9, 9, 0, 1]).unwrap();

        let status = status(&repo).await.unwrap();
        let blob = status
            .files
            .iter()
            .find(|file| file.path == "blob.bin")
            .unwrap();
        assert!(blob.binary);
        assert_eq!((blob.added, blob.removed), (None, None));

        let patch = file_patch(
            &repo,
            "blob.bin",
            None,
            PatchSide::WorkingTreeVsHead,
            3,
            0,
            4000,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(patch.binary);
        assert!(!patch.patch.contains("@@"));

        std::fs::remove_dir_all(repo.parent().unwrap()).ok();
    }
}
