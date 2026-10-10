//! A delegation to another of the owner's computers. The task runs there as
//! an ordinary task of that gateway's own ledger; here it is a mirror under
//! the caller's task, moved by the peer's `task` frames while a socket is up
//! (one per peer, while any mirror of it is open) and by the driver's 60 s
//! pass when it is not. A mirror never fails because its peer went quiet:
//! it keeps its last state, and its view says when the peer was last heard.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;

use super::peer_client::{FeedItem, PeerClient, PeerError, RemoteTask, TaskFeed};
use super::peers::Peer;
use crate::tasks::domain::{
    now_ms, Requested, Task, TaskBinding, TaskChange, TaskFilter, TaskId, TaskKind, TaskState,
};
use crate::tasks::service::{DelegateRequest, TaskError, TaskView, OPEN_STATES};
use crate::AppState;

const SOURCE: &str = "peer";
/// How often a quiet socket re-checks whether any mirror of its peer is still open.
const IDLE_CHECK: Duration = Duration::from_secs(30);
/// The first wait before a failed socket is opened again, doubling up to the last.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MOST: Duration = Duration::from_secs(60);

/// Open `request` on `peer` and keep a mirror of it here, under the caller's
/// task. A peer that cannot take it leaves a failed mirror (or none, when it
/// is known to be quiet) and says why.
pub(crate) async fn delegate(
    state: &AppState,
    by: Requested,
    peer: Peer,
    request: DelegateRequest,
) -> Result<TaskView, TaskError> {
    let name = peer.name.clone();
    let Some(peer) = state.peers.checked(peer).await else {
        return Err(TaskError::PeerUnauthorized(name));
    };
    if !peer.reachable(now_ms()) {
        return Err(TaskError::MachineUnreachable(name));
    }
    let now = now_ms();
    let mirror = state
        .tasks
        .record(&Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: request.goal.clone(),
            cwd: request.cwd.clone(),
            binding: TaskBinding::None,
            state: TaskState::Queued,
            summary: None,
            exit_code: None,
            created_by: by.by,
            created_ms: now,
            updated_ms: now,
            seq: 0,
            parent: by.parent,
            origin: by.origin,
            label: request.label.clone(),
            requester_name: by.name,
            session_title: None,
        })
        .await?;
    let mut body = json!({ "goal": request.goal });
    for (key, value) in [
        ("label", &request.label),
        ("cwd", &request.cwd),
        ("agent_id", &request.agent_id),
        ("mode", &request.mode),
        ("model", &request.model),
        ("variant", &request.variant),
    ] {
        if let Some(value) = value {
            body[key] = json!(value);
        }
    }
    let created = match PeerClient::for_peer(&peer) {
        Ok(client) => client.create_task(&body).await,
        Err(_) => Err(PeerError::Unreachable("client".into())),
    };
    let remote = match created {
        Ok(remote) => remote,
        Err(error) => {
            let error = refusal(state, &peer, error).await;
            state
                .tasks
                .record_failure(&mirror, failed(&error.to_string()))
                .await;
            return Err(error);
        }
    };
    state.peers.mark_seen(&peer.peer_id).await;
    let binding = TaskBinding::Peer {
        peer_id: peer.peer_id.clone(),
        remote_task_id: remote.task_id.clone(),
        peer_name: peer.name.clone(),
    };
    let mirror = state
        .tasks
        .move_to(
            &mirror.id,
            TaskChange {
                binding: Some(binding),
                to: TaskState::Dispatching,
                source: SOURCE.into(),
                ..Default::default()
            },
        )
        .await?;
    apply(state, &mirror, &remote).await;
    watch(state, &peer.peer_id);
    let now = state
        .tasks
        .task(&mirror.id)
        .await?
        .ok_or(TaskError::NotFound)?;
    Ok(state.tasks.present(&now).await)
}

/// What the peer's refusal means for the caller; a link the peer no longer
/// knows is forgotten here, so the phone's reconciler links it afresh (§10.10).
async fn refusal(state: &AppState, peer: &Peer, error: PeerError) -> TaskError {
    let machine = peer.name.clone();
    match error {
        PeerError::Unreachable(_) => TaskError::MachineUnreachable(machine),
        PeerError::Unauthorized => {
            state.peers.forget(&peer.peer_id).await;
            TaskError::PeerUnauthorized(machine)
        }
        PeerError::Refused { code, .. } if code == "delegations_disabled" => {
            TaskError::DelegationsDisabled(machine)
        }
        PeerError::Refused { code, .. } => TaskError::PeerRefused { machine, code },
    }
}

/// How `mirror` moves to match `remote`, if at all: never backwards, never
/// out of an end, and on a new question while waiting (a new line).
pub(crate) fn mirror_change(mirror: &Task, remote: &RemoteTask) -> Option<TaskChange> {
    if mirror.state.is_terminal() {
        return None;
    }
    // A task still starting over there is dispatching here, and never goes back.
    let to = match remote.state {
        TaskState::Queued | TaskState::Dispatching if mirror.state == TaskState::Queued => {
            TaskState::Dispatching
        }
        TaskState::Queued | TaskState::Dispatching => return None,
        state => state,
    };
    let asks_anew =
        to == TaskState::Waiting && remote.summary.is_some() && remote.summary != mirror.summary;
    if to == mirror.state && !asks_anew {
        return None;
    }
    Some(TaskChange {
        to,
        source: SOURCE.into(),
        summary: remote.summary.clone(),
        exit_code: remote.exit_code,
        ..Default::default()
    })
}

/// Copy `remote` into `mirror`: its progress line, and its state when that
/// says something new. Whether the mirror moved.
pub(crate) async fn apply(state: &AppState, mirror: &Task, remote: &RemoteTask) -> bool {
    if let (Some(line), false) = (&remote.progress, remote.state.is_terminal()) {
        state.tasks.note_progress(state, &mirror.id, line).await;
    }
    let Some(change) = mirror_change(mirror, remote) else {
        return false;
    };
    match state.tasks.move_to(&mirror.id, change).await {
        Ok(_) => true,
        Err(error) => {
            tracing::debug!(task = %mirror.id.0, %error, "mirror not moved");
            false
        }
    }
}

/// Follow `peer_id`'s task socket unless a follower already does.
pub(crate) fn watch(state: &AppState, peer_id: &str) {
    let (follower, id) = (state.clone(), peer_id.to_owned());
    state
        .peers
        .sockets
        .ensure(peer_id, move || tokio::spawn(follow(follower, id)));
}

/// While a mirror of `peer_id` is open: its task socket, opened again after
/// a wait that doubles while it fails; the mirrors read over HTTP meanwhile.
/// Ends when none is open, or when the peer no longer knows this gateway.
async fn follow(state: AppState, peer_id: String) {
    let mut retry = RETRY_FIRST;
    loop {
        let Some(peer) = state.peers.get(&peer_id).await else {
            return;
        };
        let Ok(client) = PeerClient::for_peer(&peer) else {
            return;
        };
        if open_mirrors(&state, &peer_id).await.is_empty() {
            return;
        }
        match client.subscribe_tasks().await {
            Ok(mut feed) => {
                retry = RETRY_FIRST;
                state.peers.mark_seen(&peer_id).await;
                if !follow_feed(&state, &client, &peer, &mut feed).await {
                    return;
                }
            }
            Err(PeerError::Unauthorized) => {
                forget(&state, &peer_id).await;
                return;
            }
            Err(_) => {
                if !read_all(&state, &client, &peer).await {
                    return;
                }
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(RETRY_MOST);
            }
        }
    }
}

/// Apply `feed` to the open mirrors. `true`: the socket ended and is worth
/// opening again; `false`: nothing is left to follow.
async fn follow_feed(
    state: &AppState,
    client: &PeerClient,
    peer: &Peer,
    feed: &mut TaskFeed,
) -> bool {
    loop {
        let open = open_mirrors(state, &peer.peer_id).await;
        if open.is_empty() {
            return false;
        }
        match tokio::time::timeout(IDLE_CHECK, feed.next()).await {
            // Quiet, not dead: the feed ends a socket that stops answering.
            Err(_) => state.peers.mark_seen(&peer.peer_id).await,
            Ok(None) => return true,
            Ok(Some(FeedItem::Task(remote))) => {
                state.peers.mark_seen(&peer.peer_id).await;
                if let Some(mirror) = open.get(&remote.task_id) {
                    apply(state, mirror, &remote).await;
                }
            }
            // First on every socket, and when B dropped frames.
            Ok(Some(FeedItem::Resync)) => {
                if !read_all(state, client, peer).await {
                    return false;
                }
            }
        }
    }
}

/// Read every open mirror of `peer` from it. `false`: the peer no longer
/// knows this gateway and was forgotten.
async fn read_all(state: &AppState, client: &PeerClient, peer: &Peer) -> bool {
    for mirror in open_mirrors(state, &peer.peer_id).await.into_values() {
        if refresh(state, client, peer, &mirror).await.is_none() {
            return false;
        }
    }
    true
}

/// Read `mirror`'s task from `peer` and apply it; whether the mirror moved.
/// `None`: the peer no longer knows this gateway, which is forgotten.
async fn refresh(
    state: &AppState,
    client: &PeerClient,
    peer: &Peer,
    mirror: &Task,
) -> Option<bool> {
    match client.read_task(remote_id(mirror)).await {
        Ok(remote) => {
            state.peers.mark_seen(&peer.peer_id).await;
            Some(apply(state, mirror, &remote).await)
        }
        Err(PeerError::Refused { status: 404, .. }) => {
            state.peers.mark_seen(&peer.peer_id).await;
            let lost = failed(&format!("lost: {} no longer has this task", peer.name));
            state.tasks.record_failure(mirror, lost).await;
            Some(true)
        }
        Err(PeerError::Unauthorized) => {
            forget(state, &peer.peer_id).await;
            None
        }
        // Quiet: the mirror keeps its state. Published anyway, so its view's
        // `peer_reachable`/`peer_last_seen_ms` (§10.12) reach the phone and
        // a blocked `tasks.wait` wakes to them.
        Err(_) => {
            state.tasks.republish(mirror).await;
            Some(false)
        }
    }
}

/// The peer answered 401: forget it (§10.10) and say so on its mirrors,
/// which keep their state until the user cancels or marks them done.
async fn forget(state: &AppState, peer_id: &str) {
    let open = open_mirrors(state, peer_id).await;
    tracing::info!(peer = %peer_id, "a peer no longer knows this gateway; forgetting it");
    state.peers.forget(peer_id).await;
    for mirror in open.values() {
        state.tasks.republish(mirror).await;
    }
}

/// The open mirrors of `peer_id`, by the id of the task they mirror.
async fn open_mirrors(state: &AppState, peer_id: &str) -> HashMap<String, Task> {
    let filter = TaskFilter {
        states: Some(OPEN_STATES.to_vec()),
        peer: Some(peer_id.to_owned()),
        limit: 1000,
        ..Default::default()
    };
    state
        .tasks
        .open_tasks(filter)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|task| (remote_id(&task).to_owned(), task))
        .collect()
}

fn remote_id(task: &Task) -> &str {
    match &task.binding {
        TaskBinding::Peer { remote_task_id, .. } => remote_task_id,
        _ => "",
    }
}

/// The driver's pass for a mirror no socket follows: read it from its peer,
/// then follow the peer again. Whether the mirror moved.
pub(crate) async fn converge(state: &AppState, mirror: &Task) -> bool {
    let TaskBinding::Peer { peer_id, .. } = &mirror.binding else {
        return false;
    };
    if state.peers.sockets.is_live(peer_id) {
        return false;
    }
    let Some(peer) = state.peers.get(peer_id).await else {
        // Forgotten: its view says unreachable now; so does the phone's row.
        state.tasks.republish(mirror).await;
        return false;
    };
    let Ok(client) = PeerClient::for_peer(&peer) else {
        return false;
    };
    let Some(moved) = refresh(state, &client, &peer, mirror).await else {
        return false;
    };
    watch(state, peer_id);
    moved
}

/// Ask the peer to cancel the task `mirror` mirrors, best effort: the mirror
/// is cancelled here either way.
pub(crate) async fn forward_cancel(state: &AppState, mirror: &Task) {
    let TaskBinding::Peer {
        peer_id,
        remote_task_id,
        ..
    } = &mirror.binding
    else {
        return;
    };
    let Some(peer) = state.peers.get(peer_id).await else {
        return;
    };
    if let Ok(client) = PeerClient::for_peer(&peer) {
        if let Err(error) = client.cancel_task(remote_task_id).await {
            tracing::warn!(task = %mirror.id.0, peer = %peer.peer_id, %error, "the cancel did not reach the peer");
        }
    }
}

/// `peer_id` was unlinked: its open mirrors end as lost.
pub(crate) async fn lose(state: &AppState, peer_id: &str, name: &str) {
    for mirror in open_mirrors(state, peer_id).await.into_values() {
        let lost = failed(&format!("lost: {name} was unlinked"));
        state.tasks.record_failure(&mirror, lost).await;
    }
}

fn failed(why: &str) -> TaskChange {
    TaskChange {
        to: TaskState::Failed,
        source: SOURCE.into(),
        note: Some(why.to_owned()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::mcp::tools::tests_support::delegate_as;
    use crate::tasks::domain::{PrincipalKind, TaskOrigin, TaskPrincipal};
    use crate::tasks::service::Outcome;
    use crate::test_support::{
        named, output_state, peer_router, serve, test_peer_device, FakeAgent, FakeHerdr,
    };

    fn mirror(state: TaskState, summary: Option<&str>) -> Task {
        Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "g".into(),
            cwd: None,
            binding: TaskBinding::Peer {
                peer_id: "srv-mini".into(),
                remote_task_id: "t_r".into(),
                peer_name: "mini".into(),
            },
            state,
            summary: summary.map(Into::into),
            exit_code: None,
            created_by: TaskPrincipal {
                kind: PrincipalKind::Assistant,
                id: "a".into(),
            },
            created_ms: 1,
            updated_ms: 1,
            seq: 0,
            parent: None,
            origin: TaskOrigin::Agent,
            label: None,
            requester_name: None,
            session_title: None,
        }
    }

    fn remote(state: TaskState, summary: Option<&str>) -> RemoteTask {
        RemoteTask {
            task_id: "t_r".into(),
            state,
            summary: summary.map(Into::into),
            exit_code: None,
            progress: None,
        }
    }

    fn peer(id: &str, name: &str, url: &str, seen: Option<u64>) -> Peer {
        Peer {
            peer_id: id.into(),
            server_id: id.into(),
            name: name.into(),
            url: url.into(),
            token: "peer-token".into(),
            device_id: "coding-1".into(),
            transport_key: None,
            added_ms: 0,
            last_seen_ms: seen,
            capabilities: vec![],
            manifest: None,
        }
    }

    #[test]
    fn a_mirror_moves_only_when_the_remote_task_says_something_new() {
        use TaskState::*;
        for (now, said, there, says, expected) in [
            (Dispatching, None, Working, None, Some(Working)),
            (Dispatching, None, Queued, None, None),
            (Working, None, Dispatching, None, None),
            (Working, None, Working, Some("still going"), None),
            (Working, None, Waiting, Some("Allow edit?"), Some(Waiting)),
            (
                Waiting,
                Some("Allow edit?"),
                Waiting,
                Some("Allow edit?"),
                None,
            ),
            (
                Waiting,
                Some("Allow edit?"),
                Waiting,
                Some("Which branch?"),
                Some(Waiting),
            ),
            (Waiting, Some("Allow edit?"), Working, None, Some(Working)),
            (
                Working,
                None,
                IdleUnconfirmed,
                Some("tests pass"),
                Some(IdleUnconfirmed),
            ),
            (Working, None, Done, Some("screenshot saved"), Some(Done)),
            (Done, Some("ok"), Failed, Some("late"), None),
            (Working, None, Cancelled, None, Some(Cancelled)),
        ] {
            let change = mirror_change(&mirror(now, said), &remote(there, says));
            assert_eq!(
                change.as_ref().map(|c| c.to),
                expected,
                "{now:?}/{said:?} <- {there:?}/{says:?}"
            );
            if let Some(change) = change {
                assert_eq!(
                    change.summary.as_deref(),
                    says,
                    "the remote line travels with the move"
                );
            }
        }
    }

    /// §5.2 on two in-process gateways: coding's agent hands a goal to mini,
    /// mini runs it as its own task, every move on mini reaches the mirror
    /// over the task socket, the 60 s pass catches up a mirror whose socket
    /// is down, and a cancel on the mirror reaches mini.
    #[tokio::test]
    async fn a_delegation_to_another_computer_follows_its_task_there() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let mini = named(output_state(&herdr), "srv-mini", "mini");
        mini.agent_runtime
            .attach_for_test(
                FakeAgent::new("opencode")
                    .with_sessions(&[("ses_mini", 1)])
                    .manager(),
            )
            .await;
        mini.devices
            .lock()
            .unwrap()
            .push(test_peer_device("coding-1", "peer-token"));
        let url = serve(peer_router(mini.clone())).await;
        let coding = named(output_state(&herdr), "srv-coding", "coding");
        coding
            .peers
            .store_for_test(peer("srv-mini", "mini", &url, Some(now_ms())))
            .await;

        let view = delegate_as(
            &coding,
            "MINI",
            "install build 42 on the iPad simulator; screenshot Home",
        )
        .await
        .unwrap();
        assert_eq!(view["binding"]["kind"], "peer");
        assert_eq!(view["binding"]["peer_id"], "srv-mini");
        assert_eq!(view["binding"]["peer_name"], "mini");
        assert_eq!(view["peer_reachable"], true);
        let (mirror_id, remote_id) = ids(&view);
        let there = mini.tasks.get_for_test(&remote_id).await;
        assert_eq!(
            (there.origin, there.state),
            (TaskOrigin::Peer, TaskState::Working)
        );

        // A frame: mini's agent finishes while coding's wait blocks (§10.14).
        let (finisher, finish_id) = (mini.clone(), remote_id.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            finisher
                .tasks
                .complete(&finish_id, "screenshot saved", Outcome::Done)
                .await
                .unwrap();
        });
        let by = owner(&coding, &mirror_id).await;
        let waited = coding
            .tasks
            .wait(&coding, &by, &mirror_id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            (waited.task.state, waited.task.summary.as_deref()),
            (TaskState::Done, Some("screenshot saved"))
        );

        // No socket: the driver's pass reads mini and catches up.
        let view = delegate_as(&coding, "mini", "second goal").await.unwrap();
        let (mirror_id, remote_id) = ids(&view);
        coding.peers.sockets.stop("srv-mini");
        mini.tasks
            .complete(&remote_id, "second done", Outcome::Done)
            .await
            .unwrap();
        crate::tasks::driver::reconcile(&coding).await;
        assert_eq!(
            coding.tasks.get_for_test(&mirror_id).await.state,
            TaskState::Done
        );

        // A cancel on the mirror stops the task on mini.
        let view = delegate_as(&coding, "mini", "third goal").await.unwrap();
        let (mirror_id, remote_id) = ids(&view);
        let by = owner(&coding, &mirror_id).await;
        assert_eq!(
            coding
                .tasks
                .cancel(&coding, &by, &mirror_id)
                .await
                .unwrap()
                .state,
            TaskState::Cancelled
        );
        assert_eq!(
            mini.tasks.get_for_test(&remote_id).await.state,
            TaskState::Cancelled
        );

        // Unlinked: what is still open over there ends here as lost.
        let view = delegate_as(&coding, "mini", "fourth goal").await.unwrap();
        let (mirror_id, _) = ids(&view);
        assert!(coding.peers.unlink(&coding, "srv-mini").await.unwrap());
        let lost = coding.tasks.get_for_test(&mirror_id).await;
        assert_eq!(lost.state, TaskState::Failed);
        assert_eq!(lost.summary.as_deref(), Some("lost: mini was unlinked"));
    }

    /// What the calling agent is told when the computer it names cannot take the work.
    #[tokio::test]
    async fn a_delegation_that_cannot_start_says_why_and_leaves_no_open_mirror() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let mini = named(output_state(&herdr), "srv-mini", "mini");
        mini.devices
            .lock()
            .unwrap()
            .push(test_peer_device("coding-1", "peer-token"));
        mini.peers.set_accepts_delegations(false).unwrap();
        let url = serve(peer_router(mini.clone())).await;
        let coding = named(output_state(&herdr), "srv-coding", "coding");
        coding
            .peers
            .store_for_test(peer("srv-mini", "mini", &url, Some(now_ms())))
            .await;
        coding
            .peers
            .store_for_test(peer("srv-nas", "nas", "http://127.0.0.1:9", None))
            .await;
        // A link the other computer revoked: forgotten here (§10.10).
        let old = Peer {
            token: "revoked".into(),
            ..peer("srv-old", "old", &url, Some(now_ms()))
        };
        coding.peers.store_for_test(old).await;
        for (machine, code) in [
            ("printer", "machine_unknown"),
            ("nas", "machine_unreachable"),
            ("mini", "delegations_disabled"),
            ("old", "unauthorized"),
        ] {
            let error = delegate_as(&coding, machine, "goal").await.unwrap_err();
            assert_eq!(error["error"], code, "{machine}: {error}");
            if code == "machine_unknown" {
                let message = error["message"].as_str().unwrap();
                assert!(
                    message.contains("mini") && message.contains("nas"),
                    "{message}"
                );
            }
        }
        assert!(
            coding.peers.get("srv-old").await.is_none(),
            "a revoked link is forgotten"
        );
        let open = coding
            .tasks
            .open_tasks(TaskFilter {
                states: Some(OPEN_STATES.to_vec()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            open.is_empty(),
            "a refused delegation leaves a failed mirror, never an open one"
        );
    }

    /// What a mirror says about its computer (§10.12): structured fields,
    /// never a gateway-made line; the phone words them.
    #[tokio::test]
    async fn a_mirror_says_whether_its_computer_answers() {
        let state = crate::test_support::test_state("admin", vec![]);
        let now = now_ms();
        for (seen, reachable) in [
            (Some(now), true),
            (Some(now - 5 * 60_000), false),
            (None, false),
        ] {
            state
                .peers
                .store_for_test(peer("srv-mini", "mini", "http://127.0.0.1:9", seen))
                .await;
            let id = state
                .tasks
                .insert_for_test(&mirror(TaskState::Working, None))
                .await;
            let view = state
                .tasks
                .present(&state.tasks.get_for_test(&id).await)
                .await;
            assert_eq!(
                (view.peer_reachable, view.peer_last_seen_ms),
                (Some(reachable), seen),
                "{seen:?}"
            );
            assert_eq!(view.progress, None, "no gateway-made progress line");
        }
    }

    fn ids(view: &Value) -> (TaskId, TaskId) {
        (
            TaskId(view["task_id"].as_str().unwrap().into()),
            TaskId(view["binding"]["remote_task_id"].as_str().unwrap().into()),
        )
    }

    async fn owner(state: &AppState, id: &TaskId) -> TaskPrincipal {
        state.tasks.get_for_test(id).await.created_by
    }
}
