use std::sync::Arc;

use crate::agents::domain::{
    AgentCatalog, AgentSessionId, AgentSessionInfo, ModelRef, SessionPage, SessionQuery,
};
use crate::agents::ports::agent::{AgentError, AgentPort, FileDiffItem};
use crate::agents::ports::mirror::{AgentSessionSnapshot, SessionMirrorPort};

pub struct SessionService {
    agent: Arc<dyn AgentPort>,
    mirror: Arc<dyn SessionMirrorPort>,
    /// The concrete mirror, for the few operations that are not part of the
    /// port because nothing else implements them.
    memory: Option<Arc<crate::agents::adapters::memory_mirror::MemoryMirror>>,
    refresh_slots: tokio::sync::Semaphore,
}

impl SessionService {
    pub fn new(agent: Arc<dyn AgentPort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self {
            agent,
            mirror,
            memory: None,
            refresh_slots: tokio::sync::Semaphore::new(4),
        }
    }

    /// Build a service that can also reach the in-memory mirror directly.
    pub fn with_memory_mirror(
        agent: Arc<dyn AgentPort>,
        mirror: Arc<crate::agents::adapters::memory_mirror::MemoryMirror>,
    ) -> Self {
        Self {
            agent,
            mirror: mirror.clone(),
            memory: Some(mirror),
            refresh_slots: tokio::sync::Semaphore::new(4),
        }
    }

    async fn mirror_pending(
        &self,
        asid: &AgentSessionId,
        permissions: Vec<crate::agents::domain::PermissionRequest>,
        forms: Vec<crate::agents::domain::FormRequest>,
    ) {
        if let Some(ref memory) = self.memory {
            memory.replace_pending(asid, permissions, forms).await;
        }
    }

    pub async fn list_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        let mut sessions = self.agent.list_sessions(query).await?;
        self.overlay_sessions(&mut sessions).await;
        Ok(sessions)
    }

    pub async fn list_sessions_page(
        &self,
        query: &SessionQuery,
    ) -> Result<SessionPage, AgentError> {
        let mut page = self.agent.list_sessions_page(query).await?;
        self.overlay_sessions(&mut page.data).await;
        Ok(page)
    }

    async fn overlay_sessions(&self, sessions: &mut [AgentSessionInfo]) {
        for session in sessions {
            self.stamp(session);
            self.observe_runtime(session).await;
            if let Some(memory) = &self.memory {
                memory.overlay_read_markers(session).await;
            }
        }
    }

    async fn observe_runtime(&self, info: &mut AgentSessionInfo) {
        if let Ok(Some(runtime)) = self.agent.session_runtime(&info.asid.0).await {
            runtime.apply(info);
        }
    }

    /// Tag a session with the agent that owns it. The one place agent
    /// results gain `agent_id`; adapters and routes never set it.
    fn stamp(&self, info: &mut AgentSessionInfo) {
        info.agent_id = self.agent.kind().to_string();
    }

    /// One session as the agent reports it, tagged with its agent.
    pub async fn get_session(&self, asid: &str) -> Result<AgentSessionInfo, AgentError> {
        let mut info = self.agent.get_session(asid).await?;
        self.stamp(&mut info);
        self.observe_runtime(&mut info).await;
        if let Some(memory) = &self.memory {
            memory.overlay_read_markers(&mut info).await;
        }
        Ok(info)
    }

    pub async fn create_session(
        &self,
        directory: Option<&str>,
        model: Option<&ModelRef>,
        mode: Option<&str>,
    ) -> Result<AgentSessionInfo, AgentError> {
        let mut info = self.agent.create_session(directory, model, mode).await?;
        self.stamp(&mut info);
        self.observe_runtime(&mut info).await;
        self.mirror.update_session(info.clone()).await;
        if let Some(memory) = &self.memory {
            memory.mark_history_initialized(&info.asid).await;
        }
        Ok(info)
    }

    pub async fn fork_session(
        &self,
        asid: &str,
        message_id: Option<&str>,
    ) -> Result<AgentSessionInfo, AgentError> {
        let mut info = self.agent.fork_session(asid, message_id).await?;
        if info.asid.0.is_empty() || info.asid.0 == asid {
            return Err(AgentError::Protocol(
                "Fork did not return a new session identity".into(),
            ));
        }
        self.stamp(&mut info);
        self.observe_runtime(&mut info).await;
        self.mirror.update_session(info.clone()).await;
        Ok(info)
    }

    pub async fn get_snapshot(
        &self,
        asid: &AgentSessionId,
    ) -> Result<AgentSessionSnapshot, AgentError> {
        // Mark-viewed and metadata events may already have seeded the mirror.
        // Ownership recovery belongs to selection, not a transcript cache miss.
        // A rejected preparation leaves the retained snapshot untouched.
        self.agent.prepare_selected_session(&asid.0).await?;
        let history_gate = if let Some(memory) = &self.memory {
            Some(memory.history_gate(asid).await)
        } else {
            None
        };
        let _history_read = match &history_gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        // Known empty history is cached too. Metadata or a live delta alone
        // cannot stand in for the first canonical history read.
        let initialized = match &self.memory {
            Some(memory) => memory.history_initialized(asid).await,
            None => true,
        };
        if let Some(mut snapshot) = self.mirror.get_snapshot(asid).await.filter(|_| initialized) {
            self.observe_runtime(&mut snapshot.info).await;
            return Ok(snapshot);
        }

        let before_read = if let Some(memory) = &self.memory {
            memory.history_read_revision(asid).await.unwrap_or_default()
        } else {
            (0, 0)
        };
        // The cold reads at once: the session, its newest history, and what
        // it still has pending. The revision above was taken before the
        // history read, as it was when these ran one after another.
        let (info, prepared, permissions, forms) = tokio::join!(
            self.agent.get_session(&asid.0),
            self.agent.prepare_timeline(&asid.0, 100),
            self.agent.get_pending_permissions(&asid.0),
            self.agent.get_pending_forms(&asid.0),
        );
        let mut info = info?;
        self.stamp(&mut info);
        self.observe_runtime(&mut info).await;
        self.mirror.update_session(info.clone()).await;
        let prepared = prepared?;
        let timeline = prepared.items;
        if let (Some(memory), Some(gate)) = (&self.memory, &history_gate) {
            if !memory
                .initialize_prepared_history(
                    asid,
                    gate,
                    before_read,
                    timeline.clone(),
                    prepared.commit,
                )
                .await?
            {
                return Err(AgentError::NotAvailable(
                    "Session changed during initial history read".into(),
                ));
            }
        } else {
            if prepared.commit.is_some() {
                return Err(AgentError::Unsupported(
                    "stateful history requires atomic mirror acceptance".into(),
                ));
            }
            if !timeline.is_empty() {
                self.mirror
                    .upsert_timeline_items(asid, timeline.clone())
                    .await;
            }
        }

        // Anything raised while the stream was down was read back above:
        // `/api/event` is volatile and drops what happened during a
        // disconnect, so a permission prompt could otherwise be lost for good.
        let permissions = permissions.unwrap_or_else(|err| {
            tracing::debug!(asid = %asid.0, %err, "pending permission catch-up failed");
            Vec::new()
        });
        let forms = forms.unwrap_or_else(|err| {
            tracing::debug!(asid = %asid.0, %err, "pending form catch-up failed");
            Vec::new()
        });
        if !permissions.is_empty() || !forms.is_empty() {
            self.mirror_pending(asid, permissions, forms).await;
        }

        if let Some(mut snapshot) = self.mirror.get_snapshot(asid).await {
            self.observe_runtime(&mut snapshot.info).await;
            Ok(snapshot)
        } else {
            Ok(AgentSessionSnapshot {
                info,
                timeline,
                permissions: Vec::new(),
                forms: Vec::new(),
                inbox: Vec::new(),
                seq: 1,
            })
        }
    }

    /// Explicit user refresh. Ordinary snapshot reads keep their retained rows.
    pub async fn refresh_snapshot(
        &self,
        asid: &AgentSessionId,
    ) -> Result<AgentSessionSnapshot, AgentError> {
        let _permit = self
            .refresh_slots
            .try_acquire()
            .map_err(|_| AgentError::NotAvailable("Session refresh capacity exhausted".into()))?;
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.refresh_snapshot_inner(asid),
        )
        .await
        .map_err(|_| AgentError::NotAvailable("Session refresh timed out".into()))?
    }

    async fn refresh_snapshot_inner(
        &self,
        asid: &AgentSessionId,
    ) -> Result<AgentSessionSnapshot, AgentError> {
        self.agent.prepare_selected_session(&asid.0).await?;
        let memory = self
            .memory
            .as_ref()
            .ok_or_else(|| AgentError::Unsupported("explicit session refresh".into()))?;
        let (expected, incarnation) = memory.snapshot_read_revision(asid).await;
        let info = self.get_session(&asid.0).await?;
        let prepared = self.agent.prepare_timeline(&asid.0, 100).await?;
        let fresh = prepared.items;
        let mut timeline = fresh.clone();
        if let Some(held) = self.mirror.get_snapshot(asid).await {
            let fresh_ids: std::collections::HashSet<_> =
                fresh.iter().map(|item| item.id.clone()).collect();
            timeline.extend(
                held.timeline
                    .into_iter()
                    .filter(|item| !fresh_ids.contains(&item.id)),
            );
            timeline.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        }
        let permissions = self.agent.get_pending_permissions(&asid.0).await?;
        let forms = self.agent.get_pending_forms(&asid.0).await?;
        let snapshot = AgentSessionSnapshot {
            info,
            timeline,
            permissions,
            forms,
            inbox: Vec::new(),
            seq: 0,
        };
        memory
            .refresh_prepared_snapshot(snapshot, expected, incarnation, prepared.commit)
            .await?
            .ok_or_else(|| {
                AgentError::NotAvailable("Session changed during refresh; refresh again".into())
            })
    }

    /// One older history page, kept in the mirror so a later snapshot still has it.
    pub async fn load_earlier_timeline(
        &self,
        asid: &AgentSessionId,
    ) -> Result<Vec<crate::agents::domain::TimelineItem>, AgentError> {
        let items = self.agent.load_earlier_timeline(&asid.0, 80).await?;
        if !items.is_empty() {
            self.mirror.upsert_timeline_items(asid, items.clone()).await;
        }
        Ok(items)
    }

    /// Re-read the permissions and forms OpenCode still considers pending.
    pub async fn catch_up_pending(&self, asid: &AgentSessionId) {
        let permissions = self
            .agent
            .get_pending_permissions(&asid.0)
            .await
            .unwrap_or_else(|err| {
                tracing::debug!(asid = %asid.0, %err, "pending permission catch-up failed");
                Vec::new()
            });
        let forms = self
            .agent
            .get_pending_forms(&asid.0)
            .await
            .unwrap_or_else(|err| {
                tracing::debug!(asid = %asid.0, %err, "pending form catch-up failed");
                Vec::new()
            });
        if permissions.is_empty() && forms.is_empty() {
            return;
        }
        self.mirror_pending(asid, permissions, forms).await;
    }

    pub async fn get_events_after(
        &self,
        asid: &AgentSessionId,
        after_seq: u64,
    ) -> Option<Vec<crate::agents::domain::AgentDomainEvent>> {
        self.mirror.get_events_after(asid, after_seq).await
    }

    pub async fn switch_model(
        &self,
        asid: &AgentSessionId,
        model: &ModelRef,
    ) -> Result<(), AgentError> {
        self.agent.switch_model(&asid.0, model).await
    }

    pub async fn get_catalog(&self, directory: Option<&str>) -> Result<AgentCatalog, AgentError> {
        self.agent.get_catalog(directory).await
    }

    pub async fn get_vcs_diff(
        &self,
        asid: &AgentSessionId,
        mode: &str,
    ) -> Result<Vec<FileDiffItem>, AgentError> {
        self.agent.get_vcs_diff(&asid.0, mode).await
    }

    pub async fn revert_session(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
    ) -> Result<(), AgentError> {
        self.agent.revert_session(&asid.0, message_id).await?;
        let timeline = self
            .agent
            .get_timeline(&asid.0, 100)
            .await
            .unwrap_or_default();
        if !timeline.is_empty() {
            self.mirror.upsert_timeline_items(asid, timeline).await;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agents::domain::*;
    use crate::agents::ports::agent::AgentFuture;
    use std::sync::Mutex;

    /// A page-capable fake kept here so route tests exercise the same service.
    pub(crate) struct PageAgent {
        pub inner: crate::test_support::FakeAgent,
        pub queries: Arc<Mutex<Vec<(bool, SessionQuery)>>>,
    }

    impl PageAgent {
        pub fn new(kind: &'static str) -> Self {
            Self {
                inner: crate::test_support::FakeAgent::new(kind)
                    .with_sessions(&[("older", 10), ("newer", 30)]),
                queries: Arc::default(),
            }
        }
    }

    macro_rules! delegate {
        ($($name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {
            $(fn $name<'a>(&'a self, $($arg: $ty),*) -> AgentFuture<'a, $out> {
                self.inner.$name($($arg),*)
            })*
        };
    }

    impl AgentPort for PageAgent {
        fn kind(&self) -> &'static str {
            self.inner.kind
        }

        fn list_sessions<'a>(
            &'a self,
            query: &'a SessionQuery,
        ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
            self.queries.lock().unwrap().push((false, query.clone()));
            self.inner.list_sessions(query)
        }

        fn list_sessions_page<'a>(
            &'a self,
            query: &'a SessionQuery,
        ) -> AgentFuture<'a, SessionPage> {
            self.queries.lock().unwrap().push((true, query.clone()));
            Box::pin(async move {
                Ok(SessionPage {
                    data: self.inner.sessions.clone(),
                    cursor: SessionCursor {
                        next: Some("next+/=".into()),
                        previous: Some("previous-/=".into()),
                    },
                })
            })
        }

        delegate! {
            probe() -> bool;
            list_projects() -> Vec<AgentProject>;
            create_session(directory: Option<&'a str>, model: Option<&'a ModelRef>, mode: Option<&'a str>) -> AgentSessionInfo;
            get_session(session_id: &'a str) -> AgentSessionInfo;
            send_prompt(session_id: &'a str, text: &'a str, attachments: &'a [String], delivery: Option<&'a str>) -> ();
            revert_session(session_id: &'a str, message_id: &'a str) -> ();
            interrupt(session_id: &'a str) -> ();
            switch_model(session_id: &'a str, model: &'a ModelRef) -> ();
            switch_mode(session_id: &'a str, mode: &'a str) -> ();
            find_files(query: &'a str, limit: usize, directory: Option<&'a str>) -> Vec<serde_json::Value>;
            reply_permission(session_id: &'a str, request_id: &'a str, decision: PermissionDecision, message: Option<&'a str>) -> ();
            reply_form(session_id: &'a str, form_id: &'a str, answers: serde_json::Value) -> ();
            get_catalog(directory: Option<&'a str>) -> AgentCatalog;
            get_vcs_diff(session_id: &'a str, mode: &'a str) -> Vec<FileDiffItem>;
            get_pending_permissions(session_id: &'a str) -> Vec<PermissionRequest>;
            get_pending_forms(session_id: &'a str) -> Vec<FormRequest>;
            get_timeline(session_id: &'a str, limit: usize) -> Vec<TimelineItem>;
        }
    }

    #[tokio::test]
    async fn session_page_overlays_markers_without_refetch_or_reordering() {
        let agent = Arc::new(PageAgent::new("opencode"));
        let mirror =
            Arc::new(crate::agents::adapters::memory_mirror::MemoryMirror::for_agent("opencode"));
        let mut cached = crate::test_support::fake_session("older", 999);
        cached.time_idle = Some(80);
        cached.time_viewed = Some(70);
        mirror.update_session(cached).await;
        let service = SessionService::with_memory_mirror(agent.clone(), mirror);
        let query = SessionQuery {
            limit: Some(50),
            ..Default::default()
        };
        let page = service.list_sessions_page(&query).await.unwrap();
        assert_eq!(
            page.data
                .iter()
                .map(|s| s.asid.0.as_str())
                .collect::<Vec<_>>(),
            ["older", "newer"]
        );
        assert_eq!(page.data[0].updated_ms, 10);
        assert_eq!(page.data[0].time_idle, Some(80));
        assert_eq!(page.data[0].time_viewed, Some(70));
        assert!(page.data.iter().all(|s| s.agent_id == "opencode"));
        assert_eq!(page.cursor.next.as_deref(), Some("next+/="));
        assert_eq!(page.cursor.previous.as_deref(), Some("previous-/="));
        assert_eq!(*agent.queries.lock().unwrap(), [(true, query)]);
    }
}
