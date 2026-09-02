use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{
    Agent, AgentStatus, BackendActivity, BackendActivityStream, BackendError, BackendFuture,
    BackendKind, BackendMetadata, CreateTab, CreateWorkspace, OutputFormat, OutputSource, Pane,
    PaneId, PaneOutput, PaneRange, ReadPane, SplitDirection, SplitPane, StartAgent, StartedAgent,
    Tab, TabId, TerminalBackend, Workspace, WorkspaceId,
};

/// ASCII unit separator: the one byte a pane title, a window name or a working
/// directory will not contain, which is why the `-F` formats are joined with it
/// rather than with a printable character that a user could type.
///
/// It depends on the process running under a UTF-8 locale, and not in a small
/// way: tmux replaces every byte it will not print with `_`, and with no
/// `LC_CTYPE` this separator is one of them. The rows then arrive as a single
/// field and every parse below fails. The environment is repaired at startup
/// (see `login_env`) rather than defended against here -- a locale that mangles
/// this separator mangles every non-ASCII pane title too, and no choice of
/// separator would save those.
const FIELD_SEPARATOR: char = '\u{1f}';

/// Mirrors `MAX_OUTPUT_LINES` in main. The backend clamps too so a direct
/// caller cannot ask tmux for a hundred thousand rows.
const MAX_CAPTURE_LINES: u32 = 5000;

/// Initial window size, in rows, probed on each side of a boundary to find
/// the logical-line cut it should snap to. Doubles (capped at
/// `MAX_CAPTURE_LINES`) when a probe is inconclusive. Most wrapped lines are
/// a handful of physical rows, so the common case resolves from this first,
/// smallest window at the cost of one extra `capture-pane` call.
const WRAP_SNAP_WINDOW: u32 = 64;

/// The program this adapter spawns, named once so that the code which reports
/// whether it can be found is looking for the same thing the code that runs it
/// will look for.
pub const TMUX_PROGRAM: &str = "tmux";

#[derive(Clone)]
pub struct TmuxBackend {
    binary: PathBuf,
    socket_path: Option<PathBuf>,
}

impl TmuxBackend {
    pub fn new(socket_path: Option<PathBuf>) -> Self {
        Self {
            binary: PathBuf::from(TMUX_PROGRAM),
            socket_path,
        }
    }

    #[cfg(test)]
    fn with_binary(binary: impl Into<PathBuf>, socket_path: Option<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            socket_path,
        }
    }

    async fn output<I, S>(&self, args: I) -> Result<String, BackendError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.binary);
        if let Some(socket_path) = &self.socket_path {
            command.arg("-S").arg(socket_path);
        }
        command.args(args);
        let output = command
            .output()
            .await
            .map_err(|_| BackendError::Unavailable)?;
        if !output.status.success() {
            return Err(BackendError::Refused {
                code: None,
                // tmux diagnostics name commands and targets, never pane
                // output. Clamp them before this crosses an adapter boundary.
                message: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(240)
                    .collect(),
            });
        }
        String::from_utf8(output.stdout).map_err(|_| BackendError::InvalidResponse("UTF-8 output"))
    }

    async fn output_with_stdin<I, S>(&self, args: I, input: &[u8]) -> Result<String, BackendError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.binary);
        if let Some(socket_path) = &self.socket_path {
            command.arg("-S").arg(socket_path);
        }
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| BackendError::Unavailable)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(BackendError::InvalidResponse("tmux stdin"))?;
        stdin
            .write_all(input)
            .await
            .map_err(|_| BackendError::Unavailable)?;
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|_| BackendError::Unavailable)?;
        if !output.status.success() {
            return Err(refused(&output.stderr));
        }
        String::from_utf8(output.stdout).map_err(|_| BackendError::InvalidResponse("UTF-8 output"))
    }

    async fn list_output<I, S>(&self, args: I) -> Result<String, BackendError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        match self.output(args).await {
            Ok(output) => Ok(output),
            Err(BackendError::Refused { message, .. }) if means_no_tmux_server(&message) => {
                Ok(String::new())
            }
            Err(error) => Err(error),
        }
    }

    fn format(fields: &[&str]) -> String {
        fields.join(&FIELD_SEPARATOR.to_string())
    }

    /// Wrap flags for absolute physical rows `[lo, hi)`, oldest first.
    ///
    /// Un-joined and without `-e`: this exists purely to read tmux's own
    /// per-row `W` flag (see `row_flag_is_wrapped`), never to read content.
    /// Combining `-F` with `-J` corrupts the text -- measured live, the flag
    /// column leaks into the joined line instead of being dropped -- so this
    /// must stay a separate capture from the one that serves `PaneOutput.text`.
    async fn wrap_flags(
        &self,
        pane: &PaneId,
        history_size: u32,
        lo: u32,
        hi: u32,
    ) -> Result<Vec<bool>, BackendError> {
        if lo >= hi {
            return Ok(Vec::new());
        }
        let (s, e) = capture_bounds(lo, hi, history_size);
        let output = self
            .output(&[
                "capture-pane".to_owned(),
                "-p".to_owned(),
                "-F".to_owned(),
                "-S".to_owned(),
                s.to_string(),
                "-E".to_owned(),
                e.to_string(),
                "-t".to_owned(),
                pane.as_str().to_owned(),
            ])
            .await?;
        Ok(output.lines().map(row_flag_is_wrapped).collect())
    }

    /// The largest valid cut position `<= p` (see `floor_within`), widening
    /// the probed window and retrying for as long as it stays inconclusive.
    ///
    /// The common case -- `p` already a valid cut, which is every position in
    /// content that never wraps -- resolves from the first, smallest window.
    /// The window is capped at `MAX_CAPTURE_LINES`: a single logical line
    /// wrapping across more rows than that is already a pathological input
    /// this backend clamps elsewhere, so this best-effort's at the cap rather
    /// than growing without bound.
    async fn floor_cut(
        &self,
        pane: &PaneId,
        history_size: u32,
        total: u32,
        p: u32,
    ) -> Result<u32, BackendError> {
        if p == 0 || p >= total {
            return Ok(p.min(total));
        }
        let mut window = WRAP_SNAP_WINDOW;
        loop {
            let lo = p.saturating_sub(window);
            let flags = self.wrap_flags(pane, history_size, lo, p).await?;
            if let Some(q) = floor_within(&flags, lo, p) {
                return Ok(q);
            }
            if window >= MAX_CAPTURE_LINES {
                return Ok(lo);
            }
            window = window.saturating_mul(2).min(MAX_CAPTURE_LINES);
        }
    }

    /// The smallest valid cut position `>= p` (see `ceil_within`), widening
    /// the probed window and retrying for as long as it stays inconclusive.
    async fn ceil_cut(
        &self,
        pane: &PaneId,
        history_size: u32,
        total: u32,
        p: u32,
    ) -> Result<u32, BackendError> {
        if p == 0 || p >= total {
            return Ok(p.min(total));
        }
        let mut window = WRAP_SNAP_WINDOW;
        loop {
            let hi = p.saturating_add(window).min(total);
            let flags = self.wrap_flags(pane, history_size, p - 1, hi).await?;
            let reached_total = (hi == total).then_some(total);
            if let Some(q) = ceil_within(&flags, p, reached_total) {
                return Ok(q);
            }
            if window >= MAX_CAPTURE_LINES {
                return Ok(hi);
            }
            window = window.saturating_mul(2).min(MAX_CAPTURE_LINES);
        }
    }

    /// Snap a requested `[start, end)` outward so it never splits a logical
    /// line that `-J` would otherwise have joined -- physical-row addressing
    /// and logical-line joining agree at every reported boundary.
    ///
    /// Both bounds resolve through the same position-only `floor_cut`, so two
    /// adjacent reads sharing a boundary value always resolve it to the same
    /// row: whichever page is asked for first still ends exactly where the
    /// second begins, and paging stays disjoint rather than trading a split
    /// fragment for a duplicated line. The one case that rule can't serve --
    /// `start` and `end` both landing inside the very same wrapped run, which
    /// would otherwise collapse the range to nothing -- falls back to
    /// `ceil_cut` so the read still returns that run whole rather than
    /// silently returning no content at all.
    async fn snap_to_logical_lines(
        &self,
        pane: &PaneId,
        history_size: u32,
        range: PaneRange,
    ) -> Result<PaneRange, BackendError> {
        let start = self
            .floor_cut(pane, history_size, range.total, range.start)
            .await?;
        let mut end = self
            .floor_cut(pane, history_size, range.total, range.end)
            .await?;
        if end <= start {
            end = self
                .ceil_cut(pane, history_size, range.total, range.end)
                .await?;
        }
        Ok(PaneRange {
            start,
            end,
            total: range.total,
        })
    }

    /// `(history_size, pane_height)` for one pane.
    ///
    /// `list_panes` carries both, but a single read has no reason to query every
    /// pane on the server to learn about one of them.
    async fn pane_metrics(&self, pane: &PaneId) -> Result<(u32, u32), BackendError> {
        validate_tmux_id(pane.as_str(), '%', "pane")?;
        let output = self
            .output(&[
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                pane.as_str().to_owned(),
                "-F".to_owned(),
                "#{history_size}\t#{pane_height}".to_owned(),
            ])
            .await?;
        let line = output.lines().next().unwrap_or_default();
        let mut parts = line.split('\t');
        let history = parse_u32(parts.next().unwrap_or_default(), "pane")?;
        let height = parse_u32(parts.next().unwrap_or_default(), "pane")?;
        Ok((history, height))
    }
}

impl TerminalBackend for TmuxBackend {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata> {
        Box::pin(async move {
            let version = self.output(["-V"]).await?;
            Ok(BackendMetadata {
                kind: BackendKind::Tmux,
                version: Some(
                    version
                        .trim()
                        .strip_prefix("tmux ")
                        .unwrap_or(version.trim())
                        .to_owned(),
                ),
                protocol: None,
                compatibility_response: None,
            })
        })
    }

    fn activity_stream(&self) -> BackendFuture<'_, BackendActivityStream> {
        let backend = self.clone();
        Box::pin(async move {
            let activity = async_stream::stream! {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut previous = None;
                loop {
                    interval.tick().await;
                    match (
                        backend.list_workspaces().await,
                        backend.list_tabs().await,
                        backend.list_panes().await,
                    ) {
                        (Ok(workspaces), Ok(tabs), Ok(panes)) => {
                            let fingerprint = format!("{workspaces:?}{tabs:?}{panes:?}");
                            let changed = previous.as_ref().is_some_and(|last| last != &fingerprint);
                            previous = Some(fingerprint);
                            if changed {
                                yield Ok(BackendActivity {
                                    name: "layout_updated".into(),
                                    payload: serde_json::json!({
                                        "event": "layout_updated",
                                        "data": { "backend": "tmux" }
                                    }),
                                });
                            }
                        }
                        // The failure that happened, not a word for all of
                        // them. Collapsing three different errors into
                        // `Unavailable` is how "terminal backend is
                        // unavailable" became the only thing the log could say
                        // about a tmux that was running perfectly well, and it
                        // cost days: the message named the one cause -- no tmux
                        // -- that the reader could see with their own eyes was
                        // not true, so it read as a lie rather than a clue.
                        (Err(failure), _, _) | (_, Err(failure), _) | (_, _, Err(failure)) => {
                            yield Err(failure)
                        }
                    }
                }
            };
            Ok(Box::pin(activity) as BackendActivityStream)
        })
    }

    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>> {
        Box::pin(async move {
            let format = Self::format(&[
                "#{session_id}",
                "#{session_name}",
                "#{session_attached}",
                "#{session_windows}",
                "#{window_id}",
            ]);
            let output = self.list_output(["list-sessions", "-F", &format]).await?;
            parse_rows(&output, 5, "session")?
                .into_iter()
                .map(|fields| {
                    Ok(Workspace {
                        id: WorkspaceId::new(&fields[0]),
                        number: None,
                        label: fields[1].clone(),
                        focused: parse_u32(&fields[2], "session")? > 0,
                        tab_count: Some(parse_u32(&fields[3], "session")?),
                        active_tab_id: non_empty(&fields[4]).map(TabId::new),
                        pane_count: None,
                        agent_status: AgentStatus::Unknown,
                        repo_root: None,
                        checkout_path: None,
                    })
                })
                .collect()
        })
    }

    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>> {
        Box::pin(async move {
            let format = Self::format(&[
                "#{session_id}",
                "#{window_id}",
                "#{window_name}",
                "#{window_active}",
                "#{pane_id}",
                "#{window_panes}",
            ]);
            let output = self
                .list_output(["list-windows", "-a", "-F", &format])
                .await?;
            parse_rows(&output, 6, "window")?
                .into_iter()
                .map(|fields| {
                    Ok(Tab {
                        workspace_id: WorkspaceId::new(&fields[0]),
                        id: TabId::new(&fields[1]),
                        label: fields[2].clone(),
                        focused: parse_flag(&fields[3], "window")?,
                        active_pane_id: non_empty(&fields[4]).map(PaneId::new),
                        pane_count: Some(parse_u32(&fields[5], "window")?),
                    })
                })
                .collect()
        })
    }

    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>> {
        Box::pin(async move {
            let format = Self::format(&[
                "#{session_id}",
                "#{window_id}",
                "#{pane_id}",
                "#{pane_title}",
                "#{pane_current_path}",
                "#{pane_active}",
                "#{window_active}",
                "#{session_attached}",
                "#{pane_width}",
                "#{pane_height}",
                "#{pane_current_command}",
                "#{history_size}",
                "#{alternate_on}",
                "#{pane_pid}",
            ]);
            let output = self
                .list_output(["list-panes", "-a", "-F", &format])
                .await?;
            let rows = parse_rows(&output, 14, "pane")?;
            let mut panes = Vec::with_capacity(rows.len());
            let mut unresolved = Vec::new();
            for fields in rows {
                // Kept before the row is consumed. A pane whose foreground
                // process named itself needs a second look, and this is the
                // only place the pid and the pane are in the same hand.
                let pid = fields[13].parse::<u32>().ok();
                let pane = pane_from_fields(fields)?;
                if pane.agent.is_none() {
                    if let Some(pid) = pid {
                        unresolved.push((panes.len(), pid));
                    }
                }
                panes.push(pane);
            }
            if !unresolved.is_empty() {
                let found =
                    agents_under(&unresolved.iter().map(|(_, pid)| *pid).collect::<Vec<_>>());
                for (index, pid) in unresolved {
                    if let Some(agent) = found.get(&pid) {
                        panes[index].agent = Some(agent.clone());
                    }
                }
            }
            Ok(panes)
        })
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<Agent>> {
        Box::pin(async move {
            Ok(self
                .list_panes()
                .await?
                .into_iter()
                .filter_map(agent_from_pane)
                .collect())
        })
    }

    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.list_panes()
                .await?
                .into_iter()
                .find(|pane| pane.id == *id)
                .ok_or(BackendError::InvalidTarget("pane"))
        })
    }

    fn get_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, Agent> {
        Box::pin(async move {
            let pane = self.get_pane(&PaneId::new(target)).await?;
            agent_from_pane(pane).ok_or(BackendError::InvalidTarget("agent"))
        })
    }

    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
        Box::pin(async move {
            validate_tmux_id(request.pane_id.as_str(), '%', "pane")?;

            let mut args = vec!["capture-pane".to_owned(), "-p".to_owned()];
            if request.format == OutputFormat::Ansi {
                args.push("-e".to_owned());
            }
            if request.source == OutputSource::RecentUnwrapped {
                args.push("-J".to_owned());
            }

            // `pane_metrics` costs a second tmux subprocess, so only pay for it
            // when a range actually needs resolving. The tail (no-range) path,
            // including the `Visible` hot polling path, must reach
            // `capture-pane` exactly as it did before ranges existed.
            let range = if wants_absolute_range(request.source, request.start, request.end) {
                let (history_size, pane_height) = self.pane_metrics(&request.pane_id).await?;
                let total = history_size.saturating_add(pane_height);
                let range = clamp_range(
                    request.start.expect("checked by wants_absolute_range"),
                    request.end.expect("checked by wants_absolute_range"),
                    total,
                );
                // Only `RecentUnwrapped` joins physical rows with `-J`, so
                // only it can have a boundary land inside a wrapped line
                // instead of between two logical ones. `Recent` and
                // `Detection` address the same physical rows without
                // joining, where no snap is needed: cutting a wrapped line's
                // physical rows apart there is simply reading two physical
                // rows, not splitting a logical one.
                let range = if request.source == OutputSource::RecentUnwrapped {
                    self.snap_to_logical_lines(&request.pane_id, history_size, range)
                        .await?
                } else {
                    range
                };
                let (s, e) = capture_bounds(range.start, range.end, history_size);
                args.push("-S".to_owned());
                args.push(s.to_string());
                args.push("-E".to_owned());
                args.push(e.to_string());
                Some(range)
            } else {
                if request.source != OutputSource::Visible {
                    args.push("-S".to_owned());
                    args.push(format!("-{}", request.lines));
                }
                None
            };

            args.push("-t".to_owned());
            args.push(request.pane_id.as_str().to_owned());
            let captured = strip_grid_padding(&self.output(&args).await?);

            let (text, range) = match range {
                // `capture-pane` terminates every row -- including the last --
                // with `\n`, so `captured` carries a trailing newline the tail
                // path never has (`tail_lines` builds its result with
                // `.lines()` + `.join("\n")`, which drops it). Strip that one
                // trailing `\n` so both paths return the same shape for the
                // same content; this removes a terminator, not a line, so the
                // line count `range` describes is unaffected.
                Some(range) => (strip_trailing_newline(captured), Some(range)),
                // Metrics were never fetched here, so there is nothing honest to
                // report: `PaneOutput.range` exists precisely for "this backend
                // cannot say", and inventing a range would cost a subprocess this
                // path is not allowed to spend.
                None => (tail_lines(&captured, request.lines as usize), None),
            };

            Ok(PaneOutput {
                revision: Some(text_revision(&text)),
                text,
                range,
            })
        })
    }

    fn create_workspace<'a>(
        &'a self,
        request: &'a CreateWorkspace,
    ) -> BackendFuture<'a, Workspace> {
        Box::pin(async move {
            let mut args = vec![
                "new-session".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{session_id}".to_owned(),
            ];
            if let Some(label) = request.label.as_deref() {
                args.extend(["-s".to_owned(), label.to_owned()]);
            }
            if let Some(cwd) = &request.cwd {
                args.extend(["-c".to_owned(), cwd.to_string_lossy().into_owned()]);
            }
            let raw_id = self.output(&args).await?.trim().to_owned();
            validate_tmux_id(&raw_id, '$', "workspace")?;
            let id = WorkspaceId::new(raw_id);
            if request.focus {
                self.focus_workspace(&id).await?;
            }
            self.list_workspaces()
                .await?
                .into_iter()
                .find(|workspace| workspace.id == id)
                .ok_or(BackendError::InvalidResponse("created workspace"))
        })
    }

    fn focus_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '$', "workspace")?;
            let clients = self
                .output(["list-clients", "-F", "#{client_name}"])
                .await?;
            for client in clients.lines().filter(|client| !client.is_empty()) {
                self.output(["switch-client", "-c", client, "-t", id.as_str()])
                    .await?;
            }
            Ok(())
        })
    }

    fn rename_workspace<'a>(
        &'a self,
        id: &'a WorkspaceId,
        label: &'a str,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '$', "workspace")?;
            self.output(["rename-session", "-t", id.as_str(), label])
                .await?;
            Ok(())
        })
    }

    fn close_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '$', "workspace")?;
            self.output(["kill-session", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn create_tab<'a>(&'a self, request: &'a CreateTab) -> BackendFuture<'a, Tab> {
        Box::pin(async move {
            let workspace_id = match &request.workspace_id {
                Some(id) => id.clone(),
                None => {
                    let workspaces = self.list_workspaces().await?;
                    workspaces
                        .iter()
                        .find(|workspace| workspace.focused)
                        .or_else(|| workspaces.first())
                        .map(|workspace| workspace.id.clone())
                        .ok_or(BackendError::InvalidTarget("workspace"))?
                }
            };
            validate_tmux_id(workspace_id.as_str(), '$', "workspace")?;
            let mut args = vec![
                "new-window".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{window_id}".to_owned(),
                "-t".to_owned(),
                workspace_id.as_str().to_owned(),
            ];
            if let Some(label) = request.label.as_deref() {
                args.extend(["-n".to_owned(), label.to_owned()]);
            }
            if let Some(cwd) = &request.cwd {
                args.extend(["-c".to_owned(), cwd.to_string_lossy().into_owned()]);
            }
            let raw_id = self.output(&args).await?.trim().to_owned();
            validate_tmux_id(&raw_id, '@', "tab")?;
            let id = TabId::new(raw_id);
            if request.focus {
                self.focus_tab(&id).await?;
            }
            self.list_tabs()
                .await?
                .into_iter()
                .find(|tab| tab.id == id)
                .ok_or(BackendError::InvalidResponse("created tab"))
        })
    }

    fn focus_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '@', "tab")?;
            self.output(["select-window", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn rename_tab<'a>(&'a self, id: &'a TabId, label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '@', "tab")?;
            self.output(["rename-window", "-t", id.as_str(), label])
                .await?;
            Ok(())
        })
    }

    fn close_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '@', "tab")?;
            self.output(["kill-window", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn focus_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.output(["select-pane", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn rename_pane<'a>(&'a self, id: &'a PaneId, label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.output(["select-pane", "-t", id.as_str(), "-T", label])
                .await?;
            Ok(())
        })
    }

    fn close_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.output(["kill-pane", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn split_pane<'a>(&'a self, request: &'a SplitPane) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            validate_tmux_id(request.pane_id.as_str(), '%', "pane")?;
            let mut args = vec![
                "split-window".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{pane_id}".to_owned(),
                "-t".to_owned(),
                request.pane_id.as_str().to_owned(),
                match request.direction {
                    SplitDirection::Right => "-h".to_owned(),
                    SplitDirection::Down => "-v".to_owned(),
                },
            ];
            if let Some(ratio) = request.ratio {
                if !(0.05..=0.95).contains(&ratio) {
                    return Err(BackendError::InvalidResponse("split ratio"));
                }
                args.extend([
                    "-p".to_owned(),
                    ((ratio * 100.0).round() as u32).to_string(),
                ]);
            }
            if let Some(cwd) = &request.cwd {
                args.extend(["-c".to_owned(), cwd.to_string_lossy().into_owned()]);
            }
            let raw_id = self.output(&args).await?.trim().to_owned();
            validate_tmux_id(&raw_id, '%', "pane")?;
            self.get_pane(&PaneId::new(raw_id)).await
        })
    }

    fn send_text<'a>(&'a self, id: &'a PaneId, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            let buffer = format!("gateway-{}", uuid::Uuid::new_v4());
            self.output_with_stdin(["load-buffer", "-b", &buffer, "-"], text.as_bytes())
                .await?;
            let pasted = self.output(paste_buffer_args(&buffer, id.as_str())).await;
            if pasted.is_err() {
                let _ = self.output(["delete-buffer", "-b", &buffer]).await;
            }
            pasted.map(|_| ())
        })
    }

    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            let mapped = keys
                .iter()
                .map(|key| tmux_key(key))
                .collect::<Result<Vec<_>, _>>()?;
            let mut args = vec![
                "send-keys".to_owned(),
                "-t".to_owned(),
                id.as_str().to_owned(),
            ];
            args.extend(mapped);
            self.output(&args).await?;
            Ok(())
        })
    }

    fn focus_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.focus_pane(&PaneId::new(target)).await })
    }

    fn prompt_agent<'a>(&'a self, target: &'a str, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.send_text(&PaneId::new(target), text).await })
    }

    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let executable = request
                .executable
                .as_ref()
                .ok_or_else(|| BackendError::Refused {
                    code: Some("agent_not_found".into()),
                    message: format!("agent executable {} was not found on PATH", request.command),
                })?;
            let mut argv = vec![executable.to_string_lossy().into_owned()];
            argv.extend(request.args.iter().cloned());
            let command_line = argv
                .iter()
                .map(|argument| shell_word(argument))
                .collect::<Vec<_>>()
                .join(" ");
            self.send_text(&request.pane_id, &command_line).await?;
            self.send_keys(&request.pane_id, &["Enter".to_owned()])
                .await?;

            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(request.timeout_ms);
            let executable_name = executable
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&request.command);
            loop {
                if let Ok(current) = self.get_pane(&request.pane_id).await {
                    let recognized = current.agent.as_deref() == Some(request.kind.as_str())
                        || current.foreground_command.as_deref() == Some(executable_name);
                    if recognized {
                        return Ok(StartedAgent { argv: Some(argv) });
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(BackendError::Refused {
                        code: Some("agent_start_timeout".into()),
                        message: format!(
                            "agent {} did not become visible before timeout",
                            request.kind
                        ),
                    });
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
    }

    /// Runs a raw tmux query outside of `list_output`, so "no server running"
    /// surfaces as the error it is instead of being folded into an empty
    /// topology. `list-sessions` is enough to answer the question and cheaper
    /// than listing panes; any failure *other* than the no-server message
    /// (tmux missing, some other refusal) is left alone here -- `list_panes`
    /// itself will surface that failure on its own right after this returns
    /// "reachable", so there's nothing this probe needs to diagnose twice.
    fn probe_reachable(&self) -> BackendFuture<'_, bool> {
        Box::pin(async move {
            match self.output(["list-sessions"]).await {
                Err(BackendError::Refused { message, .. }) => Ok(!means_no_tmux_server(&message)),
                _ => Ok(true),
            }
        })
    }
}

fn pane_from_fields(fields: Vec<String>) -> Result<Pane, BackendError> {
    let command = non_empty(&fields[10]);
    Ok(Pane {
        workspace_id: WorkspaceId::new(&fields[0]),
        tab_id: TabId::new(&fields[1]),
        id: PaneId::new(&fields[2]),
        terminal_id: Some(fields[2].clone()),
        label: non_empty(&fields[3]),
        terminal_title: non_empty(&fields[3]),
        cwd: non_empty(&fields[4]).map(PathBuf::from),
        focused: parse_flag(&fields[5], "pane")?
            && parse_flag(&fields[6], "pane")?
            && parse_u32(&fields[7], "pane")? > 0,
        width: Some(parse_u32(&fields[8], "pane")?),
        height: Some(parse_u32(&fields[9], "pane")?),
        revision: None,
        agent: detected_agent(command.as_deref()),
        foreground_command: command,
        agent_status: AgentStatus::Unknown,
        max_offset_from_bottom: Some(parse_u32(&fields[11], "pane")?),
        viewport_rows: Some(parse_u32(&fields[9], "pane")?),
        // Lenient, unlike `focused`'s `parse_flag`: this is a new field with
        // one internal reader (`ScrollbackStore`), and that reader already
        // treats "unknown" as "leave this pane alone" (see
        // `ScrollbackStore::observe`). An unexpected value from a tmux this
        // was never tested against should fall back to that same "unknown"
        // rather than fail the whole pane list over one optional field.
        alternate_on: match fields[12].as_str() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        },
    })
}

fn agent_from_pane(pane: Pane) -> Option<Agent> {
    let kind = pane.agent.clone()?;
    Some(Agent {
        target: pane.id.as_str().to_owned(),
        pane_id: pane.id,
        workspace_id: Some(pane.workspace_id),
        tab_id: Some(pane.tab_id),
        display_agent: Some(kind.clone()),
        kind: Some(kind),
        status: pane.agent_status,
        state_change_seq: None,
    })
}

/// Quote one argv value for the interactive shell running in a tmux pane.
/// Values are data: they are always single-quoted and never interpreted as
/// shell structure assembled by the adapter.
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The argv that delivers a loaded buffer into a pane.
///
/// `-p` does two separate jobs, and both are load-bearing. Do not drop it for
/// either one alone.
///
/// It keeps a multi-line message one message. `paste-buffer` rewrites every LF
/// in the buffer to CR, and CR is byte-for-byte what the Enter key sends, so
/// without `-p` a line-submitting TUI submits at the first embedded newline and
/// reads the rest as a second, interrupting prompt. `-p` wraps the payload in
/// the bracketed-paste markers, which say "all of this is content".
///
/// It is also the only channel by which an agent can recognise an attachment.
/// Claude Code converts an image path to an `[Image #N]` reference inside its
/// paste handler, which typed input never reaches: character-by-character input
/// and an unbracketed paste both leave the raw path in the prompt. There is no
/// fallback to lose it to -- the terminal image protocols (Kitty, Sixel, OSC
/// 1337) are output-only and OSC 52 carries text -- so without `-p` a phone that
/// attaches a photo sends a long uploads path instead of the picture.
///
/// It is safe to pass unconditionally: tmux emits the markers only for a pane
/// whose program has requested bracketed paste mode (DECSET 2004), so a program
/// that never asked receives exactly the bytes it received before, and no
/// program can be handed markers it would print as text.
fn paste_buffer_args<'a>(buffer: &'a str, pane_id: &'a str) -> [&'a str; 7] {
    ["paste-buffer", "-b", buffer, "-t", pane_id, "-d", "-p"]
}

fn refused(stderr: &[u8]) -> BackendError {
    BackendError::Refused {
        code: None,
        message: String::from_utf8_lossy(stderr)
            .trim()
            .chars()
            .take(240)
            .collect(),
    }
}

fn means_no_tmux_server(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no server running")
        || message.contains("failed to connect to server")
        // The prefix, not the reason in the parentheses. tmux prints
        // "error connecting to <socket> (<reason>)", and the reason varies with
        // why the connect failed: a missing file, a path past the 104-byte
        // sockaddr_un limit, a socket owned by somebody else. Matching the
        // reasons one at a time meant every unlisted one came back as "the
        // server is there" -- which is exactly the answer `probe_reachable`
        // exists to avoid giving. A caller cannot talk to a server tmux could
        // not connect to, whichever way the connect failed.
        || message.contains("error connecting to")
        || message.contains("no such file or directory")
}

/// A key name from a shortcut table, in tmux's spelling.
///
/// Unknown names are refused rather than passed through, and that is
/// load-bearing: `tmux send-keys` treats a name it does not recognise as
/// literal text, so handing it `shift+tab` types those nine characters into the
/// pane instead of failing. Refusing turns a wrong key into an error the caller
/// can see rather than into text in somebody's prompt.
fn tmux_key(value: &str) -> Result<String, BackendError> {
    let lower = value.to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "enter" => "Enter".to_owned(),
        "escape" | "esc" => "Escape".to_owned(),
        "tab" => "Tab".to_owned(),
        // Back-tab, which is what Shift+Tab is on the wire (`CSI Z`). tmux
        // spells it `BTab` and accepts neither `shift+tab` nor `S-Tab` for it:
        // the first becomes literal text and the second lands as a plain Tab.
        "shift+tab" => "BTab".to_owned(),
        "backspace" => "BSpace".to_owned(),
        "up" | "arrowup" => "Up".to_owned(),
        "down" | "arrowdown" => "Down".to_owned(),
        "left" | "arrowleft" => "Left".to_owned(),
        "right" | "arrowright" => "Right".to_owned(),
        _ if lower.starts_with("ctrl+") && lower.len() == 6 => {
            let key = lower.as_bytes()[5] as char;
            if !key.is_ascii_alphabetic() {
                return Err(BackendError::InvalidResponse("key name"));
            }
            format!("C-{key}")
        }
        _ if value.chars().count() == 1 && !value.chars().any(char::is_control) => value.to_owned(),
        _ => return Err(BackendError::InvalidResponse("key name")),
    };
    Ok(mapped)
}

fn parse_rows(
    output: &str,
    expected_fields: usize,
    context: &'static str,
) -> Result<Vec<Vec<String>>, BackendError> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = line
                .split(FIELD_SEPARATOR)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if fields.len() != expected_fields {
                return Err(BackendError::InvalidResponse(context));
            }
            Ok(fields)
        })
        .collect()
}

fn parse_flag(value: &str, context: &'static str) -> Result<bool, BackendError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(BackendError::InvalidResponse(context)),
    }
}

fn parse_u32(value: &str, context: &'static str) -> Result<u32, BackendError> {
    value
        .parse()
        .map_err(|_| BackendError::InvalidResponse(context))
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn validate_tmux_id(value: &str, prefix: char, kind: &'static str) -> Result<(), BackendError> {
    let mut chars = value.chars();
    if chars.next() != Some(prefix) || !chars.clone().all(|character| character.is_ascii_digit()) {
        return Err(BackendError::InvalidTarget(kind));
    }
    if chars.next().is_none() {
        return Err(BackendError::InvalidTarget(kind));
    }
    Ok(())
}

/// Whether a read actually addresses an absolute range.
///
/// Both bounds must be given, and `Visible` is the live screen: no range
/// addresses it, so a `Visible` read with a range attached still reads the
/// current screen rather than history.
fn wants_absolute_range(source: OutputSource, start: Option<u32>, end: Option<u32>) -> bool {
    source != OutputSource::Visible && start.is_some() && end.is_some()
}

/// Clamp a requested absolute `[start, end)` range to what tmux can serve:
/// never past `total`, never empty, never wider than `MAX_CAPTURE_LINES`.
fn clamp_range(start: u32, end: u32, total: u32) -> PaneRange {
    let start = start.min(total.saturating_sub(1));
    let end = end.min(total).max(start + 1);
    let end = end.min(start + MAX_CAPTURE_LINES);
    PaneRange { start, end, total }
}

/// Absolute `[start, end)` to the inclusive `-S`/`-E` pair tmux wants.
///
/// tmux numbers rows from the top of the *visible* pane: 0 is the first visible
/// row and negatives reach back into scrollback, so absolute line `i` sits at
/// `i - history_size`. `-E` is inclusive, hence the extra `- 1`.
fn capture_bounds(start: u32, end: u32, history_size: u32) -> (i64, i64) {
    let origin = i64::from(history_size);
    (
        i64::from(start) - origin,
        i64::from(end.max(start + 1)) - 1 - origin,
    )
}

/// Drop each row's trailing padding.
///
/// tmux pads rows to the pane width and `-J` preserves that padding, so a
/// 357-column pane sends a few hundred spaces after every short line. Only
/// trailing runs go: interior spacing aligns tables and leading spacing is
/// indentation, and both are content.
fn strip_grid_padding(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches([' ', '\t']));
    }
    out
}

fn tail_lines(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

/// Drop a single trailing `\n`, if present.
///
/// `capture-pane` terminates every captured row with `\n`, including the
/// last. Removing just that one terminator (never more, never a whole line)
/// makes the range-addressed `read_pane` path match the shape `tail_lines`
/// already produces, without changing how many lines the text holds.
fn strip_trailing_newline(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Whether a `capture-pane -F` row continues onto the next physical row.
///
/// `-F` prefixes each row with its flag column -- one or more characters,
/// e.g. `-`, `X`, or `WX` -- then a single space, then the row's own
/// content. `W` is tmux's own record of a wrap: measured live, it is set
/// exactly on a row whose content continues onto the next physical row
/// without a line break, and NOT set on a row that merely happens to fill
/// the pane's full width before an explicit newline. That distinction is why
/// this flag is used instead of a width heuristic, which cannot tell those
/// two cases apart.
fn row_flag_is_wrapped(line: &str) -> bool {
    line.split(' ').next().unwrap_or_default().contains('W')
}

/// The largest valid cut position `<= p`, using wrap flags already fetched
/// for absolute rows `[lo, p)` (`flags[i]` is whether row `lo + i` wraps
/// into row `lo + i + 1`).
///
/// A position `q` is a valid cut when nothing wraps into it -- cutting there
/// never splits a logical line. Content that never wraps has no positions
/// where anything wraps into it, so every `p` is already valid and this is a
/// no-op: the aligned case and all unwrapped content are unaffected.
///
/// Returns `None` when the window itself isn't wide enough to tell -- every
/// row down to `lo` is still mid-run and `lo > 0` -- so the caller must widen
/// the window and retry.
fn floor_within(flags: &[bool], lo: u32, p: u32) -> Option<u32> {
    let mut q = p;
    while q > lo {
        if !flags[(q - 1 - lo) as usize] {
            return Some(q);
        }
        q -= 1;
    }
    (lo == 0).then_some(0)
}

/// The smallest valid cut position `>= p`, using wrap flags already fetched
/// for absolute rows `[p - 1, p - 1 + flags.len())` (`flags[i]` is whether
/// row `p - 1 + i` wraps into row `p + i`).
///
/// `end_if_window_reached_total` is `Some(total)` when the fetched window
/// already reaches the pane's end, so running off the window's edge without
/// resolving still has an answer (`total` is always a valid cut). Otherwise
/// it is `None`, and the caller must widen the window and retry.
fn ceil_within(flags: &[bool], p: u32, end_if_window_reached_total: Option<u32>) -> Option<u32> {
    let mut k = 0;
    while k < flags.len() && flags[k] {
        k += 1;
    }
    if k < flags.len() {
        return Some(p + k as u32);
    }
    end_if_window_reached_total
}

fn text_revision(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

const AGENTS: &[&str] = &[
    "claude", "codex", "opencode", "qodercli", "gemini", "amp", "cursor", "pi",
];

fn detected_agent(command: Option<&str>) -> Option<String> {
    let command = command?.rsplit('/').next()?.to_ascii_lowercase();
    AGENTS
        .iter()
        .find(|agent| command == **agent || command.starts_with(&format!("{agent}-")))
        .map(|agent| (*agent).to_owned())
}

/// The agent running somewhere under each of these pane pids.
///
/// `#{pane_current_command}` is the *name* of the foreground process, and an
/// agent that renames itself is invisible to it. Claude Code is exactly that
/// case: it sets its process name to its own version, so tmux reports
/// `2.1.239` where the argv still plainly says `claude`. The pane then has no
/// agent, which costs it the agent key row -- no Shift+Tab on a Claude pane --
/// its status, and its place in the agent list.
///
/// So the name is asked of `ps`, which reports argv, and the whole subtree is
/// searched rather than just the pane's own child: a pane usually holds a shell
/// and the agent is its child, or its grandchild behind a wrapper script.
///
/// One `ps` for every unresolved pane at once, and only when at least one pane
/// went unmatched -- a machine whose panes are all shells pays nothing, and a
/// machine full of agents pays one process per listing rather than one per
/// pane.
fn agents_under(pane_pids: &[u32]) -> HashMap<u32, String> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,args="])
        .output()
    else {
        return HashMap::new();
    };
    agents_in_listing(&String::from_utf8_lossy(&output.stdout), pane_pids)
}

/// The tree walk, given a `ps` listing. Separated so it can be tested against a
/// captured one -- including the shape that started this, where the process
/// name is a version number and only the argv still says `claude`.
fn agents_in_listing(listing: &str, pane_pids: &[u32]) -> HashMap<u32, String> {
    let mut found = HashMap::new();
    let mut children: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(parent), Some(program)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(parent)) = (pid.parse::<u32>(), parent.parse::<u32>()) else {
            continue;
        };
        children
            .entry(parent)
            .or_default()
            .push((pid, program.to_owned()));
    }

    for &root in pane_pids {
        let mut queue = vec![root];
        // A pid appears once as a child of one parent, so the walk terminates
        // on the tree's own shape. The bound is belt and braces against a
        // `ps` listing that contradicts itself mid-read, which it can: it is
        // sampled, not atomic.
        let mut budget = 256;
        while let Some(pid) = queue.pop() {
            budget -= 1;
            if budget == 0 {
                break;
            }
            for (child, program) in children.get(&pid).into_iter().flatten() {
                if let Some(agent) = detected_agent(Some(program)) {
                    found.insert(root, agent);
                    queue.clear();
                    break;
                }
                queue.push(*child);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt as _;

    #[test]
    fn parses_tmux_panes_into_backend_neutral_entities() {
        let row = [
            "$0",
            "@2",
            "%9",
            "agent",
            "/work/project",
            "1",
            "1",
            "1",
            "122",
            "41",
            "claude",
            "12",
            "1",
        ]
        .join(&FIELD_SEPARATOR.to_string());
        let fields = parse_rows(&row, 13, "pane").unwrap().remove(0);
        let pane = pane_from_fields(fields).unwrap();

        assert_eq!(pane.workspace_id.as_str(), "$0");
        assert_eq!(pane.tab_id.as_str(), "@2");
        assert_eq!(pane.id.as_str(), "%9");
        assert_eq!(pane.agent.as_deref(), Some("claude"));
        assert_eq!(pane.cwd, Some(PathBuf::from("/work/project")));
        assert_eq!(pane.max_offset_from_bottom, Some(12));
        assert_eq!(pane.viewport_rows, Some(41));
        assert_eq!(pane.alternate_on, Some(true));
    }

    #[test]
    fn shift_tab_reaches_tmux_as_back_tab() {
        // `BTab` and nothing else. `S-Tab` lands as a plain Tab, and an
        // unrecognised name is not an error to tmux at all -- `send-keys`
        // types it, so passing `shift+tab` through would put those nine
        // characters in the pane.
        assert_eq!(tmux_key("shift+tab").unwrap(), "BTab");
        assert_eq!(tmux_key("Shift+Tab").unwrap(), "BTab");
        assert_eq!(tmux_key("tab").unwrap(), "Tab");
        // Still refused, rather than guessed at from the `shift+` prefix.
        assert!(tmux_key("shift+enter").is_err());
        assert!(tmux_key("shift+f5").is_err());
    }

    #[test]
    fn an_agent_that_renamed_itself_is_still_found_under_its_pane() {
        // The listing that started this, near enough verbatim. tmux reports
        // `#{pane_current_command}` as `2.1.239` for the first pane, because
        // Claude Code sets its process name to its own version -- so the pane
        // has no agent, no agent key row, and no Shift+Tab. Only the argv
        // still says what it is.
        let listing = "\
 6327     1 -zsh
 6452  6327 /Users/okk/.local/bin/claude --allow-dangerously-skip-permissions
 6539     1 -zsh
 6540  6539 /bin/sh /usr/local/bin/agent-wrapper
65231  6540 claude --dangerously-skip-permissions
 7000     1 -zsh
 7001  7000 vim README.md
";
        let found = agents_in_listing(listing, &[6327, 6539, 7000]);
        assert_eq!(found.get(&6327).map(String::as_str), Some("claude"));
        // Found two levels down, behind a wrapper script: a pane holds a
        // shell, and what the shell started is not always the agent itself.
        assert_eq!(found.get(&6539).map(String::as_str), Some("claude"));
        // A pane running something else stays a pane running something else.
        assert_eq!(found.get(&7000), None);
    }

    #[test]
    fn a_listing_that_contradicts_itself_does_not_hang_the_walk() {
        // `ps` is sampled, not atomic, so a parent and child can disagree
        // about who owns whom. The budget is what keeps that from being an
        // infinite descent rather than a missing agent.
        let listing = "\
 100   200 -zsh
 200   100 -zsh
";
        assert_eq!(agents_in_listing(listing, &[100]), HashMap::new());
    }

    #[test]
    fn an_unrecognised_alternate_on_value_is_unknown_rather_than_a_parse_failure() {
        // `alternate_on` is read leniently (see its own comment in
        // `pane_from_fields`): an unexpected value from a tmux this was never
        // tested against must not fail the whole pane list over one optional
        // field the way `focused`'s `parse_flag` is allowed to for a field
        // every caller depends on.
        let row = [
            "$0",
            "@2",
            "%9",
            "agent",
            "/work/project",
            "1",
            "1",
            "1",
            "122",
            "41",
            "claude",
            "12",
            "",
        ]
        .join(&FIELD_SEPARATOR.to_string());
        let fields = parse_rows(&row, 13, "pane").unwrap().remove(0);
        let pane = pane_from_fields(fields).unwrap();
        assert_eq!(pane.alternate_on, None);
    }

    #[test]
    fn target_ids_are_type_checked_before_reaching_tmux() {
        assert!(validate_tmux_id("%9", '%', "pane").is_ok());
        assert!(validate_tmux_id("@9", '%', "pane").is_err());
        assert!(validate_tmux_id("%9;kill-server", '%', "pane").is_err());
        assert!(validate_tmux_id("%", '%', "pane").is_err());
    }

    #[test]
    fn pasted_text_is_bracketed_so_a_newline_in_it_is_not_a_submit() {
        // Without `-p` tmux turns each LF into a CR, which a pane's program
        // cannot tell from Enter -- a two-line message then arrives as two.
        assert_eq!(
            paste_buffer_args("gateway-buf", "%9"),
            ["paste-buffer", "-b", "gateway-buf", "-t", "%9", "-d", "-p"]
        );
    }

    #[test]
    fn mobile_key_names_are_mapped_without_becoming_tmux_options() {
        assert_eq!(tmux_key("Enter").unwrap(), "Enter");
        assert_eq!(tmux_key("ctrl+c").unwrap(), "C-c");
        assert_eq!(tmux_key("ArrowUp").unwrap(), "Up");
        assert!(tmux_key("-t").is_err());
        assert!(tmux_key("C-x;kill-server").is_err());
    }

    #[test]
    fn capture_is_trimmed_to_the_requested_rows() {
        assert_eq!(tail_lines("one\ntwo\nthree\nfour\n", 2), "three\nfour");
        assert_eq!(tail_lines("one\ntwo", 10), "one\ntwo");
        assert_eq!(tail_lines("one", 0), "");
    }

    #[test]
    fn grid_padding_is_not_content() {
        // -J pads every row out to the pane width. A row of nothing but padding is
        // a blank row, and a row with padding is the row without it.
        assert_eq!(
            strip_grid_padding("text   \n      \nmore  "),
            "text\n\nmore"
        );
        // Interior spacing is content and must survive: it is what aligns a table.
        assert_eq!(strip_grid_padding("a   b   "), "a   b");
        // Leading indentation is content too.
        assert_eq!(strip_grid_padding("    indented   "), "    indented");
        assert_eq!(strip_grid_padding(""), "");
    }

    /// The app always requests `format: 'ansi'`, so every existing test above
    /// -- which feeds plain text -- proves nothing about whether padding
    /// removal runs at all in production. This fixture is shaped like what a
    /// live, isolated `tmux capture-pane -p -e -J` actually emits for a
    /// full-width colored row: verified against a real tmux 3.7b on a private
    /// socket (never the developer's default server) at a 357-column pane --
    /// the width the module doc cites -- with a reverse-video composer row
    /// and a dimmed status row, each padded to the pane width with real
    /// (written) trailing spaces.
    ///
    /// The measured placement, across many such captures: tmux always opens
    /// an SGR run right before the characters it colors and never appends a
    /// closing escape after trailing padding with nothing following it in
    /// the same row -- the reset for a colored row shows up at the *head* of
    /// whatever content needs different attributes next (another row, or
    /// nothing at all), never orphaned at this row's tail. So the escape
    /// sits before the real content, the padding trails after it same as
    /// plain text, and `trim_end_matches` reaches it exactly as it does on
    /// the plain-text fixtures above. This is not a no-op on the format the
    /// app actually uses.
    #[test]
    fn grid_padding_is_stripped_from_a_realistic_ansi_capture() {
        const PANE_WIDTH: usize = 357;
        let composer_prefix = "\x1b[7m > composer text";
        let status_prefix = "\x1b[2m auto mode on";
        let composer_row = format!(
            "{composer_prefix}{}",
            " ".repeat(PANE_WIDTH - composer_prefix.len())
        );
        let status_row = format!(
            "{status_prefix}{}",
            " ".repeat(PANE_WIDTH - status_prefix.len())
        );
        let captured = format!("{composer_row}\n{status_row}");

        let stripped = strip_grid_padding(&captured);

        assert_eq!(stripped, format!("{composer_prefix}\n{status_prefix}"));
        // The escape sequences that open each colored run survive untouched.
        assert!(stripped.contains("\x1b[7m"));
        assert!(stripped.contains("\x1b[2m"));
        // What `PaneRange` depends on: stripping padding must not change how
        // many lines the text holds.
        assert_eq!(stripped.lines().count(), captured.lines().count());

        let before = captured.len();
        let after = stripped.len();
        println!(
            "realistic ANSI capture: {before} bytes -> {after} bytes ({:.0}% removed)",
            (1.0 - after as f64 / before as f64) * 100.0
        );
    }

    #[test]
    fn agent_detection_is_conservative() {
        assert_eq!(detected_agent(Some("claude")), Some("claude".into()));
        assert_eq!(detected_agent(Some("claude-canary")), Some("claude".into()));
        assert_eq!(detected_agent(Some("bash")), None);
        assert_eq!(detected_agent(Some("notcodex")), None);
    }

    #[test]
    fn agent_arguments_are_shell_quoted_as_single_words() {
        assert_eq!(shell_word("plain"), "'plain'");
        assert_eq!(shell_word("two words"), "'two words'");
        assert_eq!(shell_word("a'b;$(touch nope)"), "'a'\\''b;$(touch nope)'");
    }

    #[test]
    fn an_absent_tmux_server_is_an_empty_topology() {
        assert!(means_no_tmux_server(
            "no server running on /tmp/tmux-1000/default"
        ));
        assert!(means_no_tmux_server(
            "error connecting to /tmp/missing.sock (No such file or directory)"
        ));
        // The same failure with a different reason in the parentheses. macOS
        // puts its temp directory 48 characters deep, so a socket named under
        // it clears the 104-byte sockaddr_un limit and tmux reports this
        // instead -- which read as "a server is running" until the check
        // stopped enumerating reasons. Real installs meet it too: any socket
        // path a user configures can be too long, or owned by somebody else.
        assert!(means_no_tmux_server(
            "error connecting to /var/folders/hw/s_30091j58v_qzbn0462b8rr0000gn/T/x.sock (File name too long)"
        ));
        assert!(means_no_tmux_server(
            "error connecting to /tmp/theirs.sock (Permission denied)"
        ));
        assert!(!means_no_tmux_server("can't find pane: %99"));
    }

    /// The exact masking the test above documents, from the other side:
    /// `list_panes` (via `list_output`) reports the same nonexistent socket
    /// as an empty topology, not an error -- correct for every other caller,
    /// but it would make a dead tmux server indistinguishable from a live one
    /// with nothing in it. `probe_reachable` exists so a caller that needs to
    /// tell those two apart can.
    #[tokio::test]
    async fn probe_reachable_tells_no_server_apart_from_list_panes_reporting_empty() {
        let socket =
            std::env::temp_dir().join(format!("gateway-probe-{}.sock", uuid::Uuid::new_v4()));
        let backend = TmuxBackend::new(Some(socket));
        assert!(!backend.probe_reachable().await.unwrap());
        assert_eq!(backend.list_panes().await.unwrap(), Vec::new());
    }

    #[test]
    fn command_runner_never_needs_a_shell() {
        let backend = TmuxBackend::with_binary("tmux", Some(PathBuf::from("/tmp/tmux.sock")));
        assert_eq!(backend.binary, PathBuf::from("tmux"));
        assert_eq!(backend.socket_path, Some(PathBuf::from("/tmp/tmux.sock")));
    }

    #[test]
    fn capture_bounds_map_absolute_lines_onto_tmux_coordinates() {
        // Measured live: history_size 463, pane_height 41, total 504.
        // tmux counts from the top of the visible pane, and -E is inclusive.
        assert_eq!(capture_bounds(0, 504, 463), (-463, 40));
        // The oldest line alone.
        assert_eq!(capture_bounds(0, 1, 463), (-463, -463));
        // The first visible row alone.
        assert_eq!(capture_bounds(463, 464, 463), (0, 0));
        // A pane with no scrollback at all.
        assert_eq!(capture_bounds(0, 41, 0), (0, 40));
    }

    #[test]
    fn wrap_flag_column_is_parsed_from_capture_pane_dash_f_output() {
        // Measured live against tmux 3.7b: `capture-pane -F` prefixes each row
        // with its flag column (possibly several characters, e.g. `WX`), a
        // single space, then the row's own content. `W` means the row
        // continues onto the next physical row.
        assert!(row_flag_is_wrapped("W line 001 xxxxx"));
        assert!(row_flag_is_wrapped("WX emoji-wrap 🎉AAA"));
        assert!(!row_flag_is_wrapped("- xxxxx"));
        assert!(!row_flag_is_wrapped("X .dotfiles main "));
        // A blank captured row is still `flag SPACE` with empty content.
        assert!(!row_flag_is_wrapped("- "));
    }

    #[test]
    fn floor_within_is_a_no_op_when_nothing_wraps() {
        // Content that never wraps must resolve to itself: the aligned case
        // and all unwrapped content must be unaffected by snapping.
        let flags = [false, false, false, false];
        assert_eq!(floor_within(&flags, 0, 4), Some(4));
        assert_eq!(floor_within(&flags, 0, 2), Some(2));
        assert_eq!(floor_within(&flags, 0, 0), Some(0));
    }

    #[test]
    fn floor_within_rounds_a_boundary_back_to_the_wrapped_lines_start() {
        // Rows 2 and 3 are one wrapped line: row 2 continues into row 3.
        // flags[i] means absolute row (lo + i) wraps into row (lo + i + 1).
        let flags = [false, false, true, false, false];
        // lo = 0, so flags[i] describes row i.
        // A boundary landing inside the wrap (row 3) must snap back to its start (row 2).
        assert_eq!(floor_within(&flags, 0, 3), Some(2));
        // A boundary already at a valid cut is left alone.
        assert_eq!(floor_within(&flags, 0, 2), Some(2));
        assert_eq!(floor_within(&flags, 0, 4), Some(4));
    }

    #[test]
    fn floor_within_returns_none_when_the_window_does_not_reach_a_resolved_boundary() {
        // The whole window, right up to its own lower edge, is one unbroken
        // wrapped run: the caller cannot tell where the run began and must
        // widen the window and retry.
        let flags = [true, true, true];
        assert_eq!(floor_within(&flags, 10, 13), None);
    }

    #[test]
    fn ceil_within_is_a_no_op_when_nothing_wraps() {
        // flags describe rows starting at (p - 1); here p = 5, so flags[0] is
        // row 4's wrap state.
        let flags = [false, false, false];
        assert_eq!(ceil_within(&flags, 5, Some(8)), Some(5));
    }

    #[test]
    fn ceil_within_rounds_a_boundary_forward_past_the_wrapped_run() {
        // p = 5 lands inside a run: row 4 (flags[0]) wraps into row 5, and
        // row 5 (flags[1]) wraps into row 6, but row 6 (flags[2]) does not
        // wrap further -- the run ends at row 6, so the next valid cut is 7.
        let flags = [true, true, false];
        assert_eq!(ceil_within(&flags, 5, Some(8)), Some(7));
    }

    #[test]
    fn ceil_within_returns_none_when_the_window_does_not_resolve_and_has_not_reached_total() {
        let flags = [true, true, true];
        assert_eq!(ceil_within(&flags, 5, None), None);
    }

    #[test]
    fn wants_absolute_range_requires_both_bounds_and_a_non_visible_source() {
        assert!(wants_absolute_range(
            OutputSource::RecentUnwrapped,
            Some(0),
            Some(10)
        ));
        assert!(wants_absolute_range(
            OutputSource::Recent,
            Some(0),
            Some(10)
        ));
        assert!(wants_absolute_range(
            OutputSource::Detection,
            Some(0),
            Some(10)
        ));
        // `Visible` is the live screen: a range attached to it is ignored.
        assert!(!wants_absolute_range(
            OutputSource::Visible,
            Some(0),
            Some(10)
        ));
        // Either bound missing means "no range requested".
        assert!(!wants_absolute_range(
            OutputSource::RecentUnwrapped,
            Some(0),
            None
        ));
        assert!(!wants_absolute_range(
            OutputSource::RecentUnwrapped,
            None,
            Some(10)
        ));
        assert!(!wants_absolute_range(
            OutputSource::RecentUnwrapped,
            None,
            None
        ));
    }

    #[test]
    fn clamp_range_keeps_a_range_that_already_fits() {
        assert_eq!(
            clamp_range(10, 20, 100),
            PaneRange {
                start: 10,
                end: 20,
                total: 100
            }
        );
    }

    #[test]
    fn clamp_range_pulls_an_overshooting_end_back_to_total_instead_of_erroring() {
        assert_eq!(
            clamp_range(90, 500, 100),
            PaneRange {
                start: 90,
                end: 100,
                total: 100
            }
        );
    }

    #[test]
    fn clamp_range_pulls_an_overshooting_start_back_to_the_newest_line() {
        assert_eq!(
            clamp_range(150, 200, 100),
            PaneRange {
                start: 99,
                end: 100,
                total: 100
            }
        );
    }

    #[test]
    fn clamp_range_caps_width_at_max_capture_lines() {
        let total = MAX_CAPTURE_LINES * 3;
        assert_eq!(
            clamp_range(0, total, total),
            PaneRange {
                start: 0,
                end: MAX_CAPTURE_LINES,
                total
            }
        );
        // The cap follows `start`, not the origin: a request for a wide slice
        // starting mid-history still serves at most `MAX_CAPTURE_LINES`.
        assert_eq!(
            clamp_range(10, total, total),
            PaneRange {
                start: 10,
                end: 10 + MAX_CAPTURE_LINES,
                total
            }
        );
    }

    /// Fresh backend on a private tmux socket, isolated from any real server.
    fn fresh_backend() -> TmuxBackend {
        let socket =
            std::env::temp_dir().join(format!("gateway-test-{}.sock", uuid::Uuid::new_v4()));
        TmuxBackend::new(Some(socket))
    }

    /// A fresh backend with one workspace already created on it, for contract
    /// tests that just need somewhere live to read from.
    async fn contract_workspace() -> (TmuxBackend, Workspace) {
        let backend = fresh_backend();
        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-contract".into()),
                focus: true,
            })
            .await
            .unwrap();
        (backend, workspace)
    }

    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn pane_metrics_report_history_and_height() {
        let (backend, workspace) = contract_workspace().await;
        let panes = backend.list_panes().await.unwrap();
        let pane = panes
            .iter()
            .find(|p| p.workspace_id == workspace.id)
            .unwrap();
        let (history, height) = backend.pane_metrics(&pane.id).await.unwrap();
        assert_eq!(Some(height), pane.viewport_rows);
        assert_eq!(Some(history), pane.max_offset_from_bottom);
        backend.close_workspace(&workspace.id).await.unwrap();
    }

    /// Poll a tmux pane format until it reads as expected, so a contract test
    /// waits on the pane's actual state instead of a sleep long enough today.
    async fn wait_for_pane_flag(
        backend: &TmuxBackend,
        pane_id: &PaneId,
        format: &str,
        expected: &str,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let value = backend
                .output(["display-message", "-p", "-t", pane_id.as_str(), format])
                .await
                .unwrap_or_default();
            if value.trim() == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{format} never became {expected}, last read {value:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Read a recording once the terminator proves the whole payload landed,
    /// rather than after a sleep that assumes it did.
    async fn read_when_complete(path: &std::path::Path, terminator: &[u8]) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let received = std::fs::read(path).unwrap_or_default();
            if received.ends_with(terminator) {
                return received;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "recording never ended with {:?}, it holds {:?}",
                String::from_utf8_lossy(terminator),
                String::from_utf8_lossy(&received)
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn a_newline_reaches_the_pane_as_pasted_content_and_not_as_enter() {
        let (backend, workspace) = contract_workspace().await;
        let panes = backend.list_panes().await.unwrap();
        let pane = panes
            .iter()
            .find(|p| p.workspace_id == workspace.id)
            .unwrap()
            .clone();

        // Read the pane's own tty rather than its rendered screen: the defect is
        // in the bytes, and a screen capture cannot tell a CR that submitted
        // from one that did not. The recorder asks for bracketed paste mode the
        // way a real TUI does, because that is the condition tmux gates the
        // markers on.
        let recording =
            std::env::temp_dir().join(format!("gateway-paste-{}", uuid::Uuid::new_v4()));
        let recorder = format!(
            "printf '\\033[?2004h'; stty raw -echo; timeout 10 cat > {}; stty sane",
            shell_word(&recording.to_string_lossy())
        );
        backend.send_text(&pane.id, &recorder).await.unwrap();
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .unwrap();

        // Wait for the mode itself, not for a guess at how long the shell takes
        // to reach the recorder. The prompt's own readline turns bracketed paste
        // off while it runs a command and the recorder turns it back on, so a
        // fixed sleep can land in that gap and read a pane that is recording but
        // has the mode off -- which is the very thing under test.
        wait_for_pane_flag(&backend, &pane.id, "#{bracket_paste_flag}", "1").await;
        backend
            .send_text(&pane.id, "first line\nsecond line")
            .await
            .unwrap();

        let received = read_when_complete(&recording, b"\x1b[201~").await;
        let _ = std::fs::remove_file(&recording);
        backend.close_workspace(&workspace.id).await.unwrap();
        assert_eq!(
            received,
            b"\x1b[200~first line\rsecond line\x1b[201~".to_vec(),
            "the pane received {:?}",
            String::from_utf8_lossy(&received)
        );
    }

    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn probe_reachable_is_true_against_a_genuinely_live_server() {
        let (backend, workspace) = contract_workspace().await;
        assert!(backend.probe_reachable().await.unwrap());
        backend.close_workspace(&workspace.id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn read_pane_serves_an_absolute_range_end_to_end() {
        let (backend, workspace) = contract_workspace().await;
        let panes = backend.list_panes().await.unwrap();
        let pane = panes
            .iter()
            .find(|p| p.workspace_id == workspace.id)
            .unwrap()
            .clone();
        backend.send_text(&pane.id, "seq 1 500").await.unwrap();
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (history, height) = backend.pane_metrics(&pane.id).await.unwrap();
        let total = history + height;

        // Full-range read should equal a huge tail read, line for line.
        let full = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: total,
                start: Some(0),
                end: Some(total),
            })
            .await
            .unwrap();
        assert_eq!(
            full.range,
            Some(PaneRange {
                start: 0,
                end: total,
                total
            })
        );
        assert_eq!(full.text.lines().count() as u32, total);

        // A middle slice returns exactly `end - start` lines, and they line up
        // with the same slice of the full read.
        let mid = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: total,
                start: Some(10),
                end: Some(20),
            })
            .await
            .unwrap();
        assert_eq!(
            mid.range,
            Some(PaneRange {
                start: 10,
                end: 20,
                total
            })
        );
        assert_eq!(mid.text.lines().count(), 10);
        let full_lines: Vec<&str> = full.text.lines().collect();
        let mid_lines: Vec<&str> = mid.text.lines().collect();
        assert_eq!(&full_lines[10..20], mid_lines.as_slice());

        // An `end` past `total` clamps rather than erroring or overshooting.
        let clamped = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: total,
                start: Some(total - 3),
                end: Some(total + 500),
            })
            .await
            .unwrap();
        assert_eq!(clamped.text.lines().count(), 3);
        assert_eq!(
            clamped.range,
            Some(PaneRange {
                start: total - 3,
                end: total,
                total
            })
        );

        // `Visible` ignores an attached range and reads the live screen.
        let visible = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::Visible,
                format: OutputFormat::Text,
                lines: 80,
                start: Some(0),
                end: Some(5),
            })
            .await
            .unwrap();
        assert_eq!(visible.range, None);
        assert_eq!(visible.text.lines().count(), height as usize);

        backend.close_workspace(&workspace.id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires permission to create a local tmux Unix socket"]
    async fn isolated_tmux_server_satisfies_the_read_write_contract() {
        if Command::new("tmux").arg("-V").output().await.is_err() {
            return;
        }
        let backend = fresh_backend();
        assert!(backend.list_workspaces().await.unwrap().is_empty());
        let mut activity = backend.activity_stream().await.unwrap();
        let activity_change = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), activity.next()).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-contract".into()),
                focus: true,
            })
            .await
            .unwrap();
        let event = activity_change.await.unwrap().unwrap().unwrap().unwrap();
        assert_eq!(event.name, "layout_updated");

        let initial_pane = backend
            .list_panes()
            .await
            .unwrap()
            .into_iter()
            .find(|pane| pane.workspace_id == workspace.id)
            .unwrap();
        let tab = backend
            .create_tab(&CreateTab {
                workspace_id: None,
                cwd: Some(std::env::temp_dir()),
                label: Some("second".into()),
                focus: false,
            })
            .await
            .unwrap();
        assert_eq!(tab.workspace_id, workspace.id);

        let split = backend
            .split_pane(&SplitPane {
                pane_id: initial_pane.id,
                direction: SplitDirection::Right,
                ratio: Some(0.5),
                cwd: Some(std::env::temp_dir()),
                env: None,
            })
            .await
            .unwrap();
        backend
            .send_text(&split.id, "printf gateway_contract_probe")
            .await
            .unwrap();
        backend
            .send_keys(&split.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let output = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::Visible,
                format: OutputFormat::Text,
                lines: 80,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(output.text.contains("gateway_contract_probe"));

        backend.send_text(&split.id, "seq 1 800").await.unwrap();
        backend
            .send_keys(&split.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let first_page = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 240,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        let third_page = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 720,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(first_page.text.lines().count(), 240);
        assert_eq!(third_page.text.lines().count(), 720);
        assert!(third_page.text.ends_with(&first_page.text));

        let metrics = backend.pane_metrics(&split.id).await.unwrap();
        let total = metrics.0 + metrics.1;
        let lower = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 0,
                start: Some(total - 200),
                end: Some(total - 100),
            })
            .await
            .unwrap();
        let upper = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 0,
                start: Some(total - 100),
                end: Some(total),
            })
            .await
            .unwrap();
        let spanning = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 0,
                start: Some(total - 200),
                end: Some(total),
            })
            .await
            .unwrap();
        // Disjoint pages tile the span exactly: this is the property the whole
        // change exists for, and the one a growing tail could never have.
        //
        // Both the tail (no-range) and range-addressed paths in `read_pane`
        // return the same shape: no trailing newline. So `lower.text` no
        // longer carries the `\n` that separated it from `upper.text` in raw
        // `capture-pane` output; that separator has to be put back explicitly
        // for the concatenation to tile the spanning read.
        assert_eq!(format!("{}\n{}", lower.text, upper.text), spanning.text);
        assert_eq!(lower.range.unwrap().end, upper.range.unwrap().start);

        backend.close_workspace(&workspace.id).await.unwrap();
    }

    /// The tiling assertion above is only proven against `seq 1 800`, which
    /// never wraps in an 80-column pane. `-J` joins wrapped *physical* rows
    /// into one logical line, but `[start, end)` still addresses physical
    /// rows (`history_size + pane_height`), so nothing stops a client-chosen
    /// boundary from landing between the two physical rows of one wrapped
    /// logical line rather than between two logical lines. This proves what
    /// happens on both sides of that boundary: aligned with the wrap, and
    /// landing inside it.
    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn range_tiling_across_a_wrapped_line_boundary() {
        let (backend, workspace) = contract_workspace().await;
        let panes = backend.list_panes().await.unwrap();
        let pane = panes
            .iter()
            .find(|p| p.workspace_id == workspace.id)
            .unwrap()
            .clone();

        // Every logical line here is 129 columns (a 9-column label plus 120
        // `x`s) in the default 80-column pane, so every one of them wraps
        // into exactly two physical rows -- a fixed, known ratio the rest of
        // this test leans on.
        const LOGICAL_LINES: usize = 60;
        backend
            .send_text(
                &pane.id,
                &format!(
                    "clear; for i in $(seq 1 {LOGICAL_LINES}); do printf 'line %03d %s\\n' \"$i\" \"$(printf 'x%.0s' {{1..120}})\"; done"
                ),
            )
            .await
            .unwrap();
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let (history, height) = backend.pane_metrics(&pane.id).await.unwrap();
        let total = history + height;

        // A physical (un-joined) read of the whole pane, so each wrapped
        // line's two physical rows come back as two separate entries and
        // `line 001`'s row can be located exactly -- `Recent` never adds
        // `-J`, unlike the `RecentUnwrapped` source everything else here
        // uses.
        let physical = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::Recent,
                format: OutputFormat::Text,
                lines: total,
                start: Some(0),
                end: Some(total),
            })
            .await
            .unwrap();
        let physical_rows: Vec<&str> = physical.text.lines().collect();
        assert_eq!(physical_rows.len() as u32, total);
        let base = physical_rows
            .iter()
            .position(|row| row.starts_with("line 001 "))
            .expect("the wrapped block's first physical row") as u32;

        let spanning = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: total,
                start: Some(0),
                end: Some(total),
            })
            .await
            .unwrap();

        let read_range = |start: u32, end: u32| {
            let backend = &backend;
            let pane_id = pane.id.clone();
            async move {
                backend
                    .read_pane(&ReadPane {
                        pane_id,
                        source: OutputSource::RecentUnwrapped,
                        format: OutputFormat::Text,
                        lines: total,
                        start: Some(start),
                        end: Some(end),
                    })
                    .await
                    .unwrap()
            }
        };

        // Aligned: the boundary sits between two logical lines (an even
        // number of physical rows into the wrapped block). Tiling holds,
        // same as the unwrapped case above.
        let aligned = base + 40;
        let lower = read_range(0, aligned).await;
        let upper = read_range(aligned, total).await;
        assert_eq!(
            format!("{}\n{}", lower.text, upper.text),
            spanning.text,
            "a boundary between two logical lines must still tile exactly"
        );

        // Misaligned: the boundary sits inside one wrapped line's two
        // physical rows. Each side of the split only ever sees its own half
        // of that capture-pane invocation, so `-J` cannot join across the
        // split on its own -- but `read_pane` now snaps a boundary landing
        // inside a wrapped line outward to the logical-line boundary it
        // belongs to, so the split never happens and tiling holds exactly,
        // the same as the aligned case above.
        let misaligned = base + 41;
        let lower = read_range(0, misaligned).await;
        let upper = read_range(misaligned, total).await;
        let concatenated = format!("{}\n{}", lower.text, upper.text);
        assert_eq!(
            concatenated, spanning.text,
            "a boundary landing inside a wrapped line must still tile exactly: read_pane \
             is expected to snap it outward to the enclosing logical line's boundary"
        );

        // `range` must keep meaning "what was served": since the snap moved
        // the actual cut from `misaligned` to the wrapped line's real
        // boundary, both sides must report that snapped position, not the
        // requested one -- and they must agree with each other, so a client
        // comparing consecutive pages' ranges sees them as genuinely
        // disjoint and adjacent, not just happens-to-tile text.
        let lower_range = lower.range.unwrap();
        let upper_range = upper.range.unwrap();
        assert_eq!(
            lower_range.end, upper_range.start,
            "snapping must report honestly: adjacent pages' reported ranges must still \
             meet exactly at the boundary that was actually served"
        );
        assert_ne!(
            lower_range.end, misaligned,
            "a snapped read must report the boundary it actually served, not the one \
             that was requested"
        );

        backend.close_workspace(&workspace.id).await.unwrap();
    }

    /// `range_tiling_across_a_wrapped_line_boundary` only ever asks for wide
    /// pages, so a boundary's `floor_cut` never lands past the start of the
    /// same wrapped run the *other* boundary already floored to -- the
    /// ordinary two-row wrap there is enough to prove tiling, but never
    /// exercises `snap_to_logical_lines`'s fallback. This drives a
    /// three-physical-row logical line and asks for a slice that starts and
    /// ends strictly inside it, touching neither the run's first row nor its
    /// last: both bounds floor to the same position (the run's start), which
    /// would otherwise collapse the read to nothing. It must instead fall
    /// back to serving the whole run.
    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn range_wholly_inside_a_wrapped_run_falls_back_to_the_whole_run() {
        let (backend, workspace) = contract_workspace().await;
        let panes = backend.list_panes().await.unwrap();
        let pane = panes
            .iter()
            .find(|p| p.workspace_id == workspace.id)
            .unwrap()
            .clone();

        // "wraprun " (8 columns) plus 221 `x`s is 229 columns: in an
        // 80-column pane that is ceil(229 / 80) = 3 physical rows, none of
        // them full only by coincidence -- the label makes the row locatable
        // and the run long enough that an interior slice can miss both ends.
        backend
            .send_text(
                &pane.id,
                "clear; printf 'wraprun %s\\n' \"$(printf 'x%.0s' {1..221})\"",
            )
            .await
            .unwrap();
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let (history, height) = backend.pane_metrics(&pane.id).await.unwrap();
        let total = history + height;

        let physical = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::Recent,
                format: OutputFormat::Text,
                lines: total,
                start: Some(0),
                end: Some(total),
            })
            .await
            .unwrap();
        let physical_rows: Vec<&str> = physical.text.lines().collect();
        let base = physical_rows
            .iter()
            .position(|row| row.starts_with("wraprun "))
            .expect("the wrapped run's first physical row") as u32;

        // The run's middle row alone: strictly inside on both sides.
        let interior = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: total,
                start: Some(base + 1),
                end: Some(base + 2),
            })
            .await
            .unwrap();

        assert_eq!(
            interior.range,
            Some(PaneRange {
                start: base,
                end: base + 3,
                total
            }),
            "a request wholly inside a wrapped run must be reported as the whole run, \
             not the interior slice that was asked for"
        );
        assert_eq!(
            interior.text.lines().count(),
            1,
            "the whole run must come back joined into its one logical line"
        );
        assert!(interior.text.starts_with("wraprun "));
        assert_eq!(interior.text.matches('x').count(), 221);

        backend.close_workspace(&workspace.id).await.unwrap();
    }
}
