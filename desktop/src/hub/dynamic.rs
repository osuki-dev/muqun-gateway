//! `DynamicHub`: a `SessionHub` that can hot-swap its underlying backend
//! (e.g. switching between local gateway and remote computers).

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use muqun_gateway::agents::directories::Completion;
use muqun_gateway::agents::ports::{AgentSessionSnapshot, FileDiffItem};
use muqun_gateway::agents::{
    AgentDomainEvent, AgentProject, AgentSessionId, AgentSessionInfo, ModelRef, PermissionDecision,
    SessionQuery, TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::platform::StoredUpload;
use tokio::sync::{broadcast, watch};

use super::{
    CatalogView, HubFuture, HubMode, PeerMention, SavedPermission, SearchPage, SearchQuery,
    SessionHub, ShellRow, TaskDefaults, TaskHubEvent, TaskRow, TaskStep, TimelineCatchUp,
    WorktreeRow,
};

const EVENT_CAPACITY: usize = 4096;

#[derive(Clone)]
pub struct ConnectionContext {
    pub computer: String,
    pub generation: u64,
    pub(crate) hub: Arc<dyn SessionHub>,
}

tokio::task_local! {
    static CONNECTION: ConnectionContext;
}

impl ConnectionContext {
    pub async fn scope<F: std::future::Future>(self, future: F) -> F::Output {
        CONNECTION.scope(self, future).await
    }
}

pub fn connection_origin() -> Option<(String, u64)> {
    CONNECTION
        .try_with(|c| (c.computer.clone(), c.generation))
        .ok()
}

/// True when the current task-local hub connection targets this computer
/// (or when no connection origin is bound, e.g. standalone mode or unit tests).
pub fn is_local_connection() -> bool {
    connection_origin().is_none_or(|(c, _)| c == "local" || c == "self")
}

/// Child reads retain their connection even if the UI changes computers.
pub fn spawn_connected<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let context = CONNECTION.try_with(Clone::clone).ok();
    tokio::spawn(async move {
        match context {
            Some(context) => context.scope(future).await,
            None => future.await,
        }
    })
}

pub struct DynamicHub {
    current: RwLock<Arc<dyn SessionHub>>,
    events: broadcast::Sender<AgentDomainEvent>,
    task_events: broadcast::Sender<TaskHubEvent>,
    forward_abort: Mutex<Option<tokio::task::AbortHandle>>,
    task_forward_abort: Mutex<Option<tokio::task::AbortHandle>>,
    link: watch::Sender<Option<String>>,
    link_forward_abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl DynamicHub {
    pub fn connection(&self, computer: String, generation: u64) -> ConnectionContext {
        ConnectionContext {
            computer,
            generation,
            hub: self.current.read().unwrap().clone(),
        }
    }
    pub fn new(initial: Arc<dyn SessionHub>) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let (task_events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let hub = Arc::new(Self {
            current: RwLock::new(initial.clone()),
            events: events_tx,
            task_events: task_events_tx,
            forward_abort: Mutex::new(None),
            task_forward_abort: Mutex::new(None),
            link: watch::channel(None).0,
            link_forward_abort: Mutex::new(None),
        });
        hub.bind_forwarders(&initial);
        hub
    }

    pub fn switch(&self, new_hub: Arc<dyn SessionHub>) {
        {
            let mut cur = self.current.write().unwrap();
            *cur = new_hub.clone();
        }
        self.bind_forwarders(&new_hub);
    }

    fn bind_forwarders(&self, hub: &Arc<dyn SessionHub>) {
        // Abort previous forwarders
        if let Some(handle) = self.forward_abort.lock().unwrap().take() {
            handle.abort();
        }
        if let Some(handle) = self.task_forward_abort.lock().unwrap().take() {
            handle.abort();
        }
        if let Some(handle) = self.link_forward_abort.lock().unwrap().take() {
            handle.abort();
        }

        let task = tokio::spawn(forward_events(hub.events(), self.events.clone()));
        *self.forward_abort.lock().unwrap() = Some(task.abort_handle());

        if let Some(task_rx) = hub.task_events() {
            let task2 = tokio::spawn(forward_tasks(task_rx, self.task_events.clone()));
            *self.task_forward_abort.lock().unwrap() = Some(task2.abort_handle());
        }
        if let Some(mut link) = hub.link() {
            self.link.send_replace(link.borrow_and_update().clone());
            let tx = self.link.clone();
            let task = tokio::spawn(async move {
                while link.changed().await.is_ok() {
                    tx.send_replace(link.borrow_and_update().clone());
                }
            });
            *self.link_forward_abort.lock().unwrap() = Some(task.abort_handle());
        } else {
            self.link.send_replace(None);
        }
    }

    fn active(&self) -> Arc<dyn SessionHub> {
        CONNECTION
            .try_with(|c| c.hub.clone())
            .unwrap_or_else(|_| self.current.read().unwrap().clone())
    }
}

impl Drop for DynamicHub {
    fn drop(&mut self) {
        for slot in [
            &self.forward_abort,
            &self.task_forward_abort,
            &self.link_forward_abort,
        ] {
            if let Some(handle) = slot.lock().unwrap().take() {
                handle.abort();
            }
        }
    }
}

async fn forward_events(
    mut rx: broadcast::Receiver<AgentDomainEvent>,
    tx: broadcast::Sender<AgentDomainEvent>,
) {
    loop {
        let event = match rx.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => AgentDomainEvent::Resync {
                asid: AgentSessionId(String::new()),
                reason: "event forwarding lagged".into(),
            },
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let _ = tx.send(event);
    }
}

async fn forward_tasks(
    mut rx: broadcast::Receiver<TaskHubEvent>,
    tx: broadcast::Sender<TaskHubEvent>,
) {
    loop {
        let event = match rx.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => TaskHubEvent::Resync,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let _ = tx.send(event);
    }
}

impl SessionHub for DynamicHub {
    fn orchestration(
        &self,
        request: super::OrchestrationRequest,
    ) -> HubFuture<'_, serde_json::Value> {
        let active = self.active();
        Box::pin(async move { active.orchestration(request).await })
    }

    fn pending_attention(
        &self,
    ) -> HubFuture<'_, Option<Vec<muqun_gateway::agents::ports::AgentPendingSnapshot>>> {
        let active = self.active();
        Box::pin(async move { active.pending_attention().await })
    }
    fn mode(&self) -> HubMode {
        self.active().mode()
    }

    fn gateway_version(&self) -> HubFuture<'_, Option<String>> {
        let active = self.active();
        Box::pin(async move { active.gateway_version().await })
    }

    fn agents(&self) -> HubFuture<'_, Vec<AgentDiscoveryInfo>> {
        let active = self.active();
        Box::pin(async move { active.agents().await })
    }

    fn attached(&self) -> HubFuture<'_, Vec<String>> {
        let active = self.active();
        Box::pin(async move { active.attached().await })
    }

    fn sessions<'a>(
        &'a self,
        agent: &'a str,
        query: SessionQuery,
    ) -> HubFuture<'a, Vec<AgentSessionInfo>> {
        let active = self.active();
        Box::pin(async move { active.sessions(agent, query).await })
    }

    fn projects<'a>(&'a self, agent: &'a str) -> HubFuture<'a, Vec<AgentProject>> {
        let active = self.active();
        Box::pin(async move { active.projects(agent).await })
    }

    fn directories<'a>(&'a self, prefix: &'a str) -> HubFuture<'a, Completion> {
        let active = self.active();
        Box::pin(async move { active.directories(prefix).await })
    }

    fn warm_agent<'a>(
        &'a self,
        agent: &'a str,
        cwd: Option<&'a str>,
        auto_approve: bool,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.warm_agent(agent, cwd, auto_approve).await })
    }

    fn create<'a>(&'a self, agent: &'a str, cwd: &'a Path) -> HubFuture<'a, AgentSessionInfo> {
        let active = self.active();
        Box::pin(async move { active.create(agent, cwd).await })
    }

    fn create_with_options<'a>(
        &'a self,
        agent: &'a str,
        cwd: &'a Path,
        model: Option<&'a ModelRef>,
        mode: Option<&'a str>,
        auto_approve: bool,
    ) -> HubFuture<'a, AgentSessionInfo> {
        let active = self.active();
        Box::pin(async move {
            active
                .create_with_options(agent, cwd, model, mode, auto_approve)
                .await
        })
    }

    fn open<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        let active = self.active();
        Box::pin(async move { active.open(asid).await })
    }

    fn wake<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.wake(asid).await })
    }

    fn peek<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Option<AgentSessionSnapshot>> {
        let active = self.active();
        Box::pin(async move { active.peek(asid).await })
    }

    fn snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        let active = self.active();
        Box::pin(async move { active.snapshot(asid).await })
    }

    fn account_limits<'a>(
        &'a self,
        agent: &'a str,
        refresh: bool,
    ) -> HubFuture<'a, serde_json::Value> {
        let active = self.active();
        Box::pin(async move { active.account_limits(agent, refresh).await })
    }

    fn media<'a>(&'a self, path: &'a str) -> HubFuture<'a, Vec<u8>> {
        let active = self.active();
        Box::pin(async move { active.media(path).await })
    }

    fn earlier<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<TimelineItem>> {
        let active = self.active();
        Box::pin(async move { active.earlier(asid).await })
    }

    fn prompt<'a>(&'a self, asid: &'a AgentSessionId, text: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.prompt(asid, text).await })
    }

    fn interrupt<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.interrupt(asid).await })
    }

    fn reply_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
        decision: PermissionDecision,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.reply_permission(asid, request_id, decision).await })
    }

    fn reply_form<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.reply_form(asid, form_id, answers).await })
    }

    fn send<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.send(asid, text, attachments, delivery).await })
    }

    fn find_files<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
        query: &'a str,
        limit: usize,
    ) -> HubFuture<'a, Vec<serde_json::Value>> {
        let active = self.active();
        Box::pin(async move { active.find_files(agent, directory, query, limit).await })
    }

    fn catalog<'a>(
        &'a self,
        agent: &'a str,
        asid: Option<&'a AgentSessionId>,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, CatalogView> {
        let active = self.active();
        Box::pin(async move { active.catalog(agent, asid, directory).await })
    }

    fn run_command<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, serde_json::Value> {
        let active = self.active();
        Box::pin(async move { active.run_command(asid, name, arguments, delivery).await })
    }

    fn activate_skill<'a>(&'a self, asid: &'a AgentSessionId, skill: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.activate_skill(asid, skill).await })
    }

    fn switch_model<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        model: &'a ModelRef,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.switch_model(asid, model).await })
    }

    fn switch_mode<'a>(&'a self, asid: &'a AgentSessionId, mode: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.switch_mode(asid, mode).await })
    }

    fn auto_approve<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, bool> {
        let active = self.active();
        Box::pin(async move { active.auto_approve(asid).await })
    }

    fn set_auto_approve<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        enabled: bool,
    ) -> HubFuture<'a, bool> {
        let active = self.active();
        Box::pin(async move { active.set_auto_approve(asid, enabled).await })
    }

    fn rename<'a>(&'a self, asid: &'a AgentSessionId, title: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.rename(asid, title).await })
    }

    fn delete<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.delete(asid).await })
    }

    fn fork<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: Option<&'a str>,
    ) -> HubFuture<'a, AgentSessionInfo> {
        let active = self.active();
        Box::pin(async move { active.fork(asid, message_id).await })
    }

    fn inbox_cancel<'a>(&'a self, asid: &'a AgentSessionId, item: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.inbox_cancel(asid, item).await })
    }

    fn inbox_delivery<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        item: &'a str,
        delivery: &'a str,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.inbox_delivery(asid, item, delivery).await })
    }

    fn store_attachment<'a>(
        &'a self,
        name: &'a str,
        bytes: Vec<u8>,
    ) -> HubFuture<'a, StoredUpload> {
        let active = self.active();
        Box::pin(async move { active.store_attachment(name, bytes).await })
    }

    fn vcs_diff<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<FileDiffItem>> {
        let active = self.active();
        Box::pin(async move { active.vcs_diff(asid).await })
    }

    fn vcs_overview<'a>(
        &'a self,
        asid: &'a AgentSessionId,
    ) -> HubFuture<'a, super::GitRepoOverview> {
        let active = self.active();
        Box::pin(async move { active.vcs_overview(asid).await })
    }

    fn vcs_files<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        mode: &'a str,
    ) -> HubFuture<'a, super::GitChangedFiles> {
        let active = self.active();
        Box::pin(async move { active.vcs_files(asid, mode).await })
    }

    fn vcs_stage<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        paths: &'a [String],
        all: bool,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.vcs_stage(asid, paths, all).await })
    }

    fn vcs_unstage<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        paths: &'a [String],
        all: bool,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.vcs_unstage(asid, paths, all).await })
    }

    fn vcs_discard<'a>(&'a self, asid: &'a AgentSessionId, path: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.vcs_discard(asid, path).await })
    }

    fn vcs_checkout<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        branch: Option<&'a str>,
        pr_number: Option<u64>,
    ) -> HubFuture<'a, String> {
        let active = self.active();
        Box::pin(async move { active.vcs_checkout(asid, branch, pr_number).await })
    }

    fn vcs_commit<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message: &'a str,
        stage_all: bool,
    ) -> HubFuture<'a, String> {
        let active = self.active();
        Box::pin(async move { active.vcs_commit(asid, message, stage_all).await })
    }

    fn vcs_sync<'a>(&'a self, asid: &'a AgentSessionId, action: &'a str) -> HubFuture<'a, String> {
        let active = self.active();
        Box::pin(async move { active.vcs_sync(asid, action).await })
    }

    fn vcs_commits<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        offset: usize,
        limit: usize,
    ) -> HubFuture<'a, super::GitCommitsPage> {
        let active = self.active();
        Box::pin(async move { active.vcs_commits(asid, offset, limit).await })
    }

    fn vcs_prs<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        state: &'a str,
        author: &'a str,
        page: u32,
        limit: u32,
        refresh: bool,
    ) -> HubFuture<'a, super::GitPrsPage> {
        let active = self.active();
        Box::pin(async move {
            active
                .vcs_prs(asid, state, author, page, limit, refresh)
                .await
        })
    }

    fn view<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.view(asid).await })
    }

    fn stage_revert<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: &'a str,
        files: bool,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.stage_revert(asid, message_id, files).await })
    }

    fn commit_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.commit_revert(asid).await })
    }

    fn clear_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.clear_revert(asid).await })
    }

    fn compact<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.compact(asid, delivery).await })
    }

    fn descendants<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<AgentSessionInfo>> {
        let active = self.active();
        Box::pin(async move { active.descendants(asid).await })
    }

    fn list_saved_permissions<'a>(
        &'a self,
        asid: &'a AgentSessionId,
    ) -> HubFuture<'a, Vec<SavedPermission>> {
        let active = self.active();
        Box::pin(async move { active.list_saved_permissions(asid).await })
    }

    fn forget_saved_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        id: &'a str,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.forget_saved_permission(asid, id).await })
    }

    fn list_shells<'a>(
        &'a self,
        agent: &'a str,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, Vec<ShellRow>> {
        let active = self.active();
        Box::pin(async move { active.list_shells(agent, directory).await })
    }

    fn kill_shell<'a>(&'a self, agent: &'a str, id: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.kill_shell(agent, id).await })
    }

    fn background_session<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.background_session(asid).await })
    }

    fn list_worktrees<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
    ) -> HubFuture<'a, Vec<WorktreeRow>> {
        let active = self.active();
        Box::pin(async move { active.list_worktrees(agent, directory).await })
    }

    fn remove_worktree<'a>(
        &'a self,
        agent: &'a str,
        project: &'a str,
        worktree: &'a str,
        force: bool,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move {
            active
                .remove_worktree(agent, project, worktree, force)
                .await
        })
    }

    fn refresh_worktrees<'a>(&'a self, agent: &'a str, directory: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.refresh_worktrees(agent, directory).await })
    }

    fn timeline_after<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        after: u64,
    ) -> HubFuture<'a, TimelineCatchUp> {
        let active = self.active();
        Box::pin(async move { active.timeline_after(asid, after).await })
    }

    fn list_tasks<'a>(
        &'a self,
        root: Option<&'a str>,
        state: Option<&'a str>,
    ) -> HubFuture<'a, Vec<TaskRow>> {
        let active = self.active();
        Box::pin(async move { active.list_tasks(root, state).await })
    }

    fn create_task<'a>(
        &'a self,
        goal: &'a str,
        agent_id: Option<&'a str>,
        cwd: Option<&'a str>,
        peer_server_id: Option<&'a str>,
    ) -> HubFuture<'a, TaskRow> {
        let active = self.active();
        Box::pin(async move {
            active
                .create_task(goal, agent_id, cwd, peer_server_id)
                .await
        })
    }

    fn cancel_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.cancel_task(id).await })
    }

    fn complete_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.complete_task(id).await })
    }

    fn merge_task_worktree<'a>(
        &'a self,
        task_id: &'a str,
        base_dir: &'a str,
        branch: &'a str,
        worktree_dir: Option<&'a str>,
    ) -> HubFuture<'a, String> {
        let active = self.active();
        Box::pin(async move {
            active
                .merge_task_worktree(task_id, base_dir, branch, worktree_dir)
                .await
        })
    }

    fn clean_task_worktree<'a>(
        &'a self,
        task_id: &'a str,
        base_dir: &'a str,
        worktree_dir: Option<&'a str>,
        branch: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move {
            active
                .clean_task_worktree(task_id, base_dir, worktree_dir, branch)
                .await
        })
    }

    fn task_defaults(&self) -> HubFuture<'_, TaskDefaults> {
        let active = self.active();
        Box::pin(async move { active.task_defaults().await })
    }

    fn task_history<'a>(&'a self, id: &'a str) -> HubFuture<'a, Vec<TaskStep>> {
        let active = self.active();
        Box::pin(async move { active.task_history(id).await })
    }

    fn list_peers(&self) -> HubFuture<'_, Vec<PeerMention>> {
        let active = self.active();
        Box::pin(async move { active.list_peers().await })
    }

    fn search<'a>(&'a self, query: &'a SearchQuery) -> HubFuture<'a, SearchPage> {
        let active = self.active();
        Box::pin(async move { active.search(query).await })
    }

    fn read_file<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        path: &'a str,
    ) -> HubFuture<'a, super::RemoteFile> {
        let active = self.active();
        Box::pin(async move { active.read_file(asid, path).await })
    }

    fn list_mcp_upstreams(&self) -> HubFuture<'_, super::McpHubSnapshot> {
        let active = self.active();
        Box::pin(async move { active.list_mcp_upstreams().await })
    }

    fn upsert_mcp_upstream<'a>(
        &'a self,
        id: &'a str,
        body: super::McpUpstreamUpsert,
    ) -> HubFuture<'a, super::McpUpstreamView> {
        let active = self.active();
        Box::pin(async move { active.upsert_mcp_upstream(id, body).await })
    }

    fn delete_mcp_upstream<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        let active = self.active();
        Box::pin(async move { active.delete_mcp_upstream(id).await })
    }

    fn probe_mcp_upstream<'a>(&'a self, id: &'a str) -> HubFuture<'a, super::McpUpstreamView> {
        let active = self.active();
        Box::pin(async move { active.probe_mcp_upstream(id).await })
    }

    fn call_mcp_upstream_tool<'a>(
        &'a self,
        id: &'a str,
        tool: &'a str,
        arguments: serde_json::Value,
    ) -> HubFuture<'a, super::McpToolCallResult> {
        let active = self.active();
        Box::pin(async move { active.call_mcp_upstream_tool(id, tool, arguments).await })
    }

    fn events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        if let Ok(events) = CONNECTION.try_with(|c| c.hub.events()) {
            return events;
        }
        self.events.subscribe()
    }

    fn link(&self) -> Option<tokio::sync::watch::Receiver<Option<String>>> {
        if let Ok(link) = CONNECTION.try_with(|c| c.hub.link()) {
            return link;
        }
        Some(self.link.subscribe())
    }

    fn task_events(&self) -> Option<broadcast::Receiver<TaskHubEvent>> {
        if let Ok(events) = CONNECTION.try_with(|c| c.hub.task_events()) {
            return events;
        }
        Some(self.task_events.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn counting_gateway() -> (
        Arc<dyn SessionHub>,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let n = socket.read(&mut request).await.unwrap();
                if String::from_utf8_lossy(&request[..n]).starts_with("GET /api/discovery ") {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{}").await;
            }
        });
        let hub = super::super::remote::RemoteHub::start(super::super::remote::LocalGateway {
            url,
            token: "isolated-fixture".into(),
            device_id: None,
            transport_key: None,
        })
        .unwrap();
        (hub, count, server)
    }

    #[tokio::test]
    async fn commands_and_child_reads_keep_their_origin_after_switch() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (first, first_reads, first_server) = counting_gateway().await;
            let (second, second_reads, second_server) = counting_gateway().await;
            let hub = DynamicHub::new(first);
            let connection = hub.connection("local".into(), 0);
            let scoped = hub.clone();
            connection
                .scope(async move {
                    scoped.agents().await.unwrap();
                    scoped.switch(second);
                    tokio::task::yield_now().await;
                    let child = scoped.clone();
                    spawn_connected(async move {
                        child.agents().await.unwrap();
                        assert_eq!(connection_origin(), Some(("local".into(), 0)));
                    })
                    .await
                    .unwrap();
                    scoped.agents().await.unwrap();
                })
                .await;
            hub.agents().await.unwrap();
            assert_eq!(first_reads.load(std::sync::atomic::Ordering::SeqCst), 3);
            assert_eq!(second_reads.load(std::sync::atomic::Ordering::SeqCst), 1);
            first_server.abort();
            second_server.abort();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lagged_agent_forwarder_resyncs_and_keeps_delivering() {
        let (source, input) = broadcast::channel(2);
        let (output, mut received) = broadcast::channel(8);
        for n in 0..4 {
            source
                .send(AgentDomainEvent::Resync {
                    asid: AgentSessionId(format!("session-{n}")),
                    reason: "fixture".into(),
                })
                .unwrap();
        }
        let forwarder = tokio::spawn(forward_events(input, output));
        async fn next(rx: &mut broadcast::Receiver<AgentDomainEvent>) -> AgentDomainEvent {
            // A bounded wait turns the original silent exit into a failure.
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap()
        }
        assert!(matches!(next(&mut received).await,
            AgentDomainEvent::Resync { asid, .. } if asid.0.is_empty()));
        for n in 2..4 {
            assert!(matches!(next(&mut received).await,
                AgentDomainEvent::Resync { asid, .. } if asid.0 == format!("session-{n}")));
        }
        drop(source);
        forwarder.await.unwrap();
    }

    #[tokio::test]
    async fn lagged_task_forwarder_resyncs_and_keeps_delivering() {
        let (source, input) = broadcast::channel(2);
        let (output, mut received) = broadcast::channel(8);
        for n in 0..4 {
            source
                .send(TaskHubEvent::Deleted {
                    task_id: format!("task-{n}"),
                })
                .unwrap();
        }
        let forwarder = tokio::spawn(forward_tasks(input, output));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event, TaskHubEvent::Resync);
        for n in 2..4 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                event,
                TaskHubEvent::Deleted {
                    task_id: format!("task-{n}")
                }
            );
        }
        drop(source);
        forwarder.await.unwrap();
    }
}
