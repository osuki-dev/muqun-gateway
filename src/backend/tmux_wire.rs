//! Wire-safe identifiers for the tmux backend.
//!
//! tmux names panes `%N`, windows `@N`, sessions `$N`. Clients put backend ids
//! straight into URL path segments, and `%` is the percent-encoding escape
//! character: `%4` is an incomplete escape and survives decoding, but `%14`
//! is a complete one and axum decodes it to a single byte before any handler
//! sees it. Every pane numbered 10 or higher was unreachable over HTTP as a
//! result.
//!
//! [`TmuxWireIds`] is the fix's one seam: it wraps the raw tmux backend and
//! translates every id crossing the trait boundary, in both directions,
//! exactly once. Below `inner`, ids are always native (`%14`) because that is
//! the only thing the tmux binary understands. Above `self`, ids are always
//! wire form (`tp14`) because that is the only thing a URL path segment can
//! carry unencoded. Nothing else in the gateway needs to know this
//! translation exists: any caller that reaches the tmux backend through
//! [`super::registry::BackendRegistry`] gets it automatically, including
//! future handlers, because there is no other path to a tmux
//! [`TerminalBackend`].
//!
//! The mapping is a straight prefix swap and is total and reversible for any
//! id tmux can produce: `%` <-> `tp`, `@` <-> `tw`, `$` <-> `ts`. It cannot
//! collide with herdr's own ids (`p1`, `w1`, ...), which never start with
//! `t` followed by one of `p`/`w`/`s` and then only digits. A wire id this
//! gateway did not issue -- wrong marker, missing digits, stray characters --
//! is rejected with [`BackendError::InvalidTarget`] rather than coerced into
//! whatever native id it might resemble.

use std::path::PathBuf;

use super::{
    Agent, BackendActivityStream, BackendError, BackendFuture, BackendMetadata, CreateTab,
    CreateWorkspace, Pane, PaneId, PaneOutput, ReadPane, SplitPane, StartAgent, StartedAgent, Tab,
    TabId, TerminalBackend, Workspace, WorkspaceId, Worktree, WorktreePlacement, WorktreeRequest,
};

/// Wraps a tmux [`TerminalBackend`] so every id crossing it is in wire form.
pub struct TmuxWireIds {
    inner: Box<dyn TerminalBackend>,
}

impl TmuxWireIds {
    pub fn new(inner: Box<dyn TerminalBackend>) -> Self {
        Self { inner }
    }
}

/// Native tmux id -> wire id. Total: any id tmux hands back starts with one
/// of the three markers below, and anything else is passed through unchanged
/// rather than panicking, because this runs on data tmux itself produced, not
/// on client input (client input goes through `decode_wire`, not this).
fn encode_native(native: &str) -> String {
    match native.as_bytes().first() {
        Some(b'%') => format!("tp{}", &native[1..]),
        Some(b'@') => format!("tw{}", &native[1..]),
        Some(b'$') => format!("ts{}", &native[1..]),
        _ => native.to_owned(),
    }
}

/// Wire id -> native tmux id, for a specific expected kind. Rejects anything
/// that is not exactly `t` + marker + one-or-more-ASCII-digits: a wrong
/// marker, a missing digit run, or extra characters are all `InvalidTarget`,
/// never coerced into a native id that merely looks plausible.
fn decode_wire(wire: &str, kind: &'static str) -> Result<String, BackendError> {
    let (marker, native_prefix) = match kind {
        "pane" => ('p', '%'),
        "tab" => ('w', '@'),
        "workspace" => ('s', '$'),
        _ => return Err(BackendError::InvalidTarget(kind)),
    };
    let digits = wire
        .strip_prefix('t')
        .and_then(|rest| rest.strip_prefix(marker))
        .ok_or(BackendError::InvalidTarget(kind))?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BackendError::InvalidTarget(kind));
    }
    Ok(format!("{native_prefix}{digits}"))
}

fn encode_workspace(mut workspace: Workspace) -> Workspace {
    workspace.id = WorkspaceId::new(encode_native(workspace.id.as_str()));
    workspace.active_tab_id = workspace
        .active_tab_id
        .map(|id| TabId::new(encode_native(id.as_str())));
    workspace
}

fn encode_tab(mut tab: Tab) -> Tab {
    tab.id = TabId::new(encode_native(tab.id.as_str()));
    tab.workspace_id = WorkspaceId::new(encode_native(tab.workspace_id.as_str()));
    tab.active_pane_id = tab
        .active_pane_id
        .map(|id| PaneId::new(encode_native(id.as_str())));
    tab
}

fn encode_pane(mut pane: Pane) -> Pane {
    pane.id = PaneId::new(encode_native(pane.id.as_str()));
    pane.workspace_id = WorkspaceId::new(encode_native(pane.workspace_id.as_str()));
    pane.tab_id = TabId::new(encode_native(pane.tab_id.as_str()));
    pane.terminal_id = pane.terminal_id.map(|id| encode_native(&id));
    pane
}

fn encode_agent(mut agent: Agent) -> Agent {
    agent.target = encode_native(&agent.target);
    agent.pane_id = PaneId::new(encode_native(agent.pane_id.as_str()));
    agent.workspace_id = agent
        .workspace_id
        .map(|id| WorkspaceId::new(encode_native(id.as_str())));
    agent.tab_id = agent
        .tab_id
        .map(|id| TabId::new(encode_native(id.as_str())));
    agent
}

fn encode_worktree_placement(mut placement: WorktreePlacement) -> WorktreePlacement {
    placement.workspace_id = WorkspaceId::new(encode_native(placement.workspace_id.as_str()));
    placement.pane_id = PaneId::new(encode_native(placement.pane_id.as_str()));
    placement
}

impl TerminalBackend for TmuxWireIds {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata> {
        self.inner.metadata()
    }

    fn activity_stream(&self) -> BackendFuture<'_, BackendActivityStream> {
        // The tmux activity stream carries no ids in its payload (just a
        // `layout_updated` marker), so there is nothing here to translate.
        self.inner.activity_stream()
    }

    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>> {
        Box::pin(async move {
            Ok(self
                .inner
                .list_workspaces()
                .await?
                .into_iter()
                .map(encode_workspace)
                .collect())
        })
    }

    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>> {
        Box::pin(async move {
            Ok(self
                .inner
                .list_tabs()
                .await?
                .into_iter()
                .map(encode_tab)
                .collect())
        })
    }

    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>> {
        Box::pin(async move {
            Ok(self
                .inner
                .list_panes()
                .await?
                .into_iter()
                .map(encode_pane)
                .collect())
        })
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<Agent>> {
        Box::pin(async move {
            Ok(self
                .inner
                .list_agents()
                .await?
                .into_iter()
                .map(encode_agent)
                .collect())
        })
    }

    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "pane")?;
            Ok(encode_pane(
                self.inner.get_pane(&PaneId::new(native)).await?,
            ))
        })
    }

    fn get_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, Agent> {
        Box::pin(async move {
            let native = decode_wire(target, "pane")?;
            Ok(encode_agent(self.inner.get_agent(&native).await?))
        })
    }

    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
        Box::pin(async move {
            let native = decode_wire(request.pane_id.as_str(), "pane")?;
            let native_request = ReadPane {
                pane_id: PaneId::new(native),
                ..request.clone()
            };
            self.inner.read_pane(&native_request).await
        })
    }

    fn create_workspace<'a>(
        &'a self,
        request: &'a CreateWorkspace,
    ) -> BackendFuture<'a, Workspace> {
        Box::pin(async move {
            Ok(encode_workspace(
                self.inner.create_workspace(request).await?,
            ))
        })
    }

    fn focus_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "workspace")?;
            self.inner.focus_workspace(&WorkspaceId::new(native)).await
        })
    }

    fn rename_workspace<'a>(
        &'a self,
        id: &'a WorkspaceId,
        label: &'a str,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "workspace")?;
            self.inner
                .rename_workspace(&WorkspaceId::new(native), label)
                .await
        })
    }

    fn close_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "workspace")?;
            self.inner.close_workspace(&WorkspaceId::new(native)).await
        })
    }

    fn create_tab<'a>(&'a self, request: &'a CreateTab) -> BackendFuture<'a, Tab> {
        Box::pin(async move {
            let native_workspace_id = match &request.workspace_id {
                Some(id) => Some(WorkspaceId::new(decode_wire(id.as_str(), "workspace")?)),
                None => None,
            };
            let native_request = CreateTab {
                workspace_id: native_workspace_id,
                ..request.clone()
            };
            Ok(encode_tab(self.inner.create_tab(&native_request).await?))
        })
    }

    fn focus_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "tab")?;
            self.inner.focus_tab(&TabId::new(native)).await
        })
    }

    fn rename_tab<'a>(&'a self, id: &'a TabId, label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "tab")?;
            self.inner.rename_tab(&TabId::new(native), label).await
        })
    }

    fn close_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "tab")?;
            self.inner.close_tab(&TabId::new(native)).await
        })
    }

    fn focus_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "pane")?;
            self.inner.focus_pane(&PaneId::new(native)).await
        })
    }

    fn rename_pane<'a>(&'a self, id: &'a PaneId, label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "pane")?;
            self.inner.rename_pane(&PaneId::new(native), label).await
        })
    }

    fn close_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "pane")?;
            self.inner.close_pane(&PaneId::new(native)).await
        })
    }

    fn split_pane<'a>(&'a self, request: &'a SplitPane) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            let native = decode_wire(request.pane_id.as_str(), "pane")?;
            let native_request = SplitPane {
                pane_id: PaneId::new(native),
                ..request.clone()
            };
            Ok(encode_pane(self.inner.split_pane(&native_request).await?))
        })
    }

    fn send_text<'a>(&'a self, id: &'a PaneId, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "pane")?;
            self.inner.send_text(&PaneId::new(native), text).await
        })
    }

    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(id.as_str(), "pane")?;
            self.inner.send_keys(&PaneId::new(native), keys).await
        })
    }

    fn focus_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(target, "pane")?;
            self.inner.focus_agent(&native).await
        })
    }

    fn prompt_agent<'a>(&'a self, target: &'a str, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let native = decode_wire(target, "pane")?;
            self.inner.prompt_agent(&native, text).await
        })
    }

    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let native = decode_wire(request.pane_id.as_str(), "pane")?;
            let native_request = StartAgent {
                pane_id: PaneId::new(native),
                ..request.clone()
            };
            self.inner.start_agent(&native_request).await
        })
    }

    fn list_worktrees<'a>(&'a self, cwd: &'a PathBuf) -> BackendFuture<'a, Vec<Worktree>> {
        // Neither side of this call carries a tmux id: `Worktree` is a path
        // and an optional branch name. Nothing to translate.
        self.inner.list_worktrees(cwd)
    }

    fn open_worktree<'a>(
        &'a self,
        request: &'a WorktreeRequest,
    ) -> BackendFuture<'a, WorktreePlacement> {
        Box::pin(async move {
            Ok(encode_worktree_placement(
                self.inner.open_worktree(request).await?,
            ))
        })
    }

    fn create_worktree<'a>(
        &'a self,
        request: &'a WorktreeRequest,
    ) -> BackendFuture<'a, WorktreePlacement> {
        Box::pin(async move {
            Ok(encode_worktree_placement(
                self.inner.create_worktree(request).await?,
            ))
        })
    }

    fn probe_reachable(&self) -> BackendFuture<'_, bool> {
        // No id on either side of this call -- nothing to translate, just
        // forward to the wrapped tmux backend's own answer.
        self.inner.probe_reachable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{OutputFormat, OutputSource};

    #[test]
    fn the_mapping_is_total_and_reversible_for_every_marker() {
        for (native, wire) in [("%14", "tp14"), ("@4", "tw4"), ("$0", "ts0"), ("%0", "tp0")] {
            assert_eq!(encode_native(native), wire);
        }
        assert_eq!(decode_wire("tp14", "pane").unwrap(), "%14");
        assert_eq!(decode_wire("tw4", "tab").unwrap(), "@4");
        assert_eq!(decode_wire("ts0", "workspace").unwrap(), "$0");
    }

    /// The whole point: a pane numbered above 9 round-trips exactly the same
    /// as one numbered below it once it is in wire form.
    #[test]
    fn double_digit_panes_round_trip_the_same_as_single_digit_ones() {
        assert_eq!(encode_native("%9"), "tp9");
        assert_eq!(encode_native("%14"), "tp14");
        assert_eq!(decode_wire("tp9", "pane").unwrap(), "%9");
        assert_eq!(decode_wire("tp14", "pane").unwrap(), "%14");
    }

    #[test]
    fn a_wire_id_this_gateway_did_not_issue_is_rejected_not_coerced() {
        // Wrong marker for the requested kind.
        assert!(decode_wire("tw14", "pane").is_err());
        // No `t` prefix at all -- including a bare native id leaking through.
        assert!(decode_wire("%14", "pane").is_err());
        // No digits after the marker.
        assert!(decode_wire("tp", "pane").is_err());
        // Non-digit trailing characters.
        assert!(decode_wire("tp14;kill-server", "pane").is_err());
        // Herdr's own id shape must never be accepted here.
        assert!(decode_wire("p1", "pane").is_err());
        assert!(decode_wire("w1", "tab").is_err());
        assert!(matches!(
            decode_wire("tw14", "pane"),
            Err(BackendError::InvalidTarget("pane"))
        ));
    }

    #[test]
    fn herdr_style_ids_cannot_collide_with_the_wire_form() {
        // herdr ids look like `p1`, `w1`, `wM:p1`: never `t` + one of our
        // markers + only digits, so they can never be mistaken for a tmux
        // wire id and never need translation of their own.
        for herdr_id in ["p1", "w1", "wM:p1", "s1"] {
            assert!(decode_wire(herdr_id, "pane").is_err());
            assert!(decode_wire(herdr_id, "tab").is_err());
            assert!(decode_wire(herdr_id, "workspace").is_err());
        }
    }

    #[test]
    fn encoding_leaves_a_non_native_string_alone() {
        // Defensive: this path only ever sees ids tmux itself produced, which
        // always carry one of the three markers. A string that somehow does
        // not is passed through rather than panicking.
        assert_eq!(encode_native("already-wire-tp14"), "already-wire-tp14");
    }

    /// Fresh wrapped backend on a private tmux socket, isolated from any real
    /// server, mirroring `tmux::tests::fresh_backend`.
    fn fresh_wrapped_backend() -> TmuxWireIds {
        let socket =
            std::env::temp_dir().join(format!("gateway-wire-test-{}.sock", uuid::Uuid::new_v4()));
        TmuxWireIds::new(Box::new(super::super::TmuxBackend::new(Some(socket))))
    }

    /// Reproduces the defect end to end against a live, isolated tmux server:
    /// create panes past the single-digit boundary that broke every double
    /// digit pane over HTTP, then read one of them back using nothing but the
    /// wire id a client would actually have. Run with
    /// `cargo test --offline -- --ignored` on an isolated socket only -- see
    /// the safety note on the crate's other tmux contract tests.
    #[tokio::test]
    #[ignore = "requires permission to create a local tmux Unix socket"]
    async fn a_pane_numbered_above_nine_is_reachable_by_its_wire_id() {
        if tokio::process::Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .is_err()
        {
            return;
        }
        let backend = fresh_wrapped_backend();
        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-wire-contract".into()),
                focus: true,
            })
            .await
            .unwrap();

        // The session opens with pane 0. tmux numbers panes globally, not per
        // window, so opening ten more windows -- each with its own
        // full-size root pane, unlike splitting one window's pane down to
        // nothing -- numbers the newest pane at 10 or above: the exact
        // boundary where the percent-encoding collision used to make a pane
        // unreachable.
        let mut pane = backend
            .list_panes()
            .await
            .unwrap()
            .into_iter()
            .find(|pane| pane.workspace_id == workspace.id)
            .unwrap();
        for _ in 0..10 {
            let tab = backend
                .create_tab(&CreateTab {
                    workspace_id: Some(workspace.id.clone()),
                    cwd: Some(std::env::temp_dir()),
                    label: None,
                    focus: false,
                })
                .await
                .unwrap();
            pane = backend
                .list_panes()
                .await
                .unwrap()
                .into_iter()
                .find(|candidate| candidate.tab_id == tab.id)
                .unwrap();
        }

        // Every id this test now touches is wire form: `tp` followed by
        // digits, and specifically double digits for the newest pane.
        assert!(pane.id.as_str().starts_with("tp"));
        let numeral: String = pane.id.as_str().trim_start_matches("tp").to_owned();
        assert!(
            numeral.len() >= 2,
            "expected a double-digit pane id, got {}",
            pane.id.as_str()
        );

        backend
            .send_text(&pane.id, "printf gateway_wire_probe")
            .await
            .unwrap();
        backend
            .send_keys(&pane.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // This is the read a client makes after decoding the URL path
        // segment for `GET /sessions/{id}/panes/{pane_id}/output`. Before the
        // fix, a client-visible id at or above `%10` never reached here
        // intact because axum decoded it before the handler saw it.
        let output = backend
            .read_pane(&ReadPane {
                pane_id: pane.id.clone(),
                source: OutputSource::Visible,
                format: OutputFormat::Text,
                lines: 80,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(output.text.contains("gateway_wire_probe"));

        // The wire id also resolves through `get_pane`, independent of the
        // read path.
        let fetched = backend.get_pane(&pane.id).await.unwrap();
        assert_eq!(fetched.id, pane.id);

        // A native id, or a made-up wire id, must not resolve to this pane:
        // the seam rejects what it did not issue rather than guessing.
        let native_form = format!("%{numeral}");
        assert!(backend.get_pane(&PaneId::new(native_form)).await.is_err());

        backend.close_workspace(&workspace.id).await.unwrap();
    }
}
