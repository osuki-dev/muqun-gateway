//! A delegation to another of the owner's computers. The task runs there as
//! an ordinary task of that gateway's own ledger; here it is a mirror under
//! the caller's task, moved by the peer's `task` frames while a socket is up
//! (one per peer, while any mirror of it is open) and by the driver's 60 s
//! pass when it is not. A mirror never fails because its peer went quiet:
//! it keeps its last state, and its view says when the peer was last heard.

use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;

use super::peer_client::{FeedItem, PeerClient, PeerError, RemoteTask, TaskFeed};
use super::peers::Peer;
use crate::tasks::domain::{
    now_ms, one_line, Requested, Task, TaskBinding, TaskChange, TaskFilter, TaskId, TaskKind,
    TaskState,
};
use crate::tasks::service::{DelegateRequest, TaskError, TaskView, OPEN_STATES, SUMMARY_CHARS};
use crate::AppState;

const SOURCE: &str = "peer";
/// How a failed mirror's summary ends after its computer's name (the one in
/// its binding), and the `reason` its view carries for it: the summary stays
/// prose, and the phone words the reason itself.
const UNLINKED_THERE: &str = " no longer accepts this computer";
const UNLINKED_HERE: &str = " was unlinked";
const NO_LONGER_LINKED: &str = " is no longer linked";
const LOST_THERE: &str = " no longer has this task";
const NOT_CONFIRMED: &str = " did not confirm the task";
const ENDINGS: [(&str, &str); 5] = [
    (UNLINKED_THERE, "peer_unlinked"),
    (UNLINKED_HERE, "peer_unlinked"),
    (NO_LONGER_LINKED, "peer_unlinked"),
    (LOST_THERE, "peer_lost_task"),
    (NOT_CONFIRMED, "peer_not_confirmed"),
];
/// How often a quiet socket re-checks whether any mirror of its peer is
/// still open, and looks for the task of a mirror whose create got no answer.
const IDLE_CHECK: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(30)
};
/// The first wait before a socket is opened again, doubling up to the last.
/// A socket that lived less than [`IDLE_CHECK`] counts as a failed one.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MOST: Duration = Duration::from_secs(60);
/// How long a mirror whose create got no answer looks for its task over
/// there before it fails as not confirmed.
const ADOPT_WITHIN: Duration = Duration::from_secs(120);

/// Open `request` on `peer` and keep a mirror of it here, under the caller's
/// task. A peer that cannot take it leaves a failed mirror (or none, when it
/// is known to be quiet) and says why. A create that got no answer leaves a
/// dispatching mirror whose task over there is still to be found
/// (`remote_task_id` empty): the next read of the peer finds it, or fails
/// the mirror after [`ADOPT_WITHIN`].
pub(crate) async fn delegate(
    state: &AppState,
    by: Requested,
    peer: Peer,
    request: DelegateRequest,
) -> Result<TaskView, TaskError> {
    let name = peer.name.clone();
    let Some(peer) = state.peers.checked(state, peer).await else {
        return Err(TaskError::PeerUnauthorized(name));
    };
    if !state.peers.reachable(&peer, now_ms()) {
        return Err(TaskError::MachineUnreachable(name));
    }
    let now = now_ms();
    // Sent with the create and kept on the mirror: how the task over there is
    // found when the create gets no answer.
    let client_ref = uuid::Uuid::new_v4().simple().to_string();
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
            mentions: request.mentions.clone(),
            client_ref: Some(client_ref.clone()),
            requester_server_id: None,
        })
        .await?;
    let mut body = json!({ "goal": request.goal, "client_ref": client_ref });
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
        Ok(remote) => Some(remote),
        Err(PeerError::NoAnswer(_)) => None,
        Err(error) => {
            let error = refusal(state, &peer, error).await;
            state
                .tasks
                .record_failure(&mirror, failed(&error.to_string()))
                .await;
            return Err(error);
        }
    };
    if remote.is_some() {
        state.peers.mark_seen(&peer.peer_id).await;
    }
    let remote_task_id = remote
        .as_ref()
        .map(|remote| remote.task_id.clone())
        .unwrap_or_default();
    let mirror = state
        .tasks
        .move_to(
            &mirror.id,
            TaskChange {
                binding: Some(binding(&peer, remote_task_id)),
                to: TaskState::Dispatching,
                source: SOURCE.into(),
                ..Default::default()
            },
        )
        .await?;
    if let Some(remote) = &remote {
        apply(state, &mirror, remote).await;
        // A frame sent before the binding above found no mirror: read once.
        if let Ok(client) = PeerClient::for_peer(&peer) {
            if let Ok(now) = client.read_task(&remote.task_id).await {
                if let Some(mirror) = state.tasks.task(&mirror.id).await? {
                    apply(state, &mirror, &now).await;
                }
            }
        }
    }
    watch(state, &peer.peer_id);
    let now = state
        .tasks
        .task(&mirror.id)
        .await?
        .ok_or(TaskError::NotFound)?;
    Ok(state.tasks.present(&now).await)
}

fn binding(peer: &Peer, remote_task_id: String) -> TaskBinding {
    TaskBinding::Peer {
        peer_id: peer.peer_id.clone(),
        peer_server_id: peer.server_id.clone(),
        remote_task_id,
        peer_name: peer.name.clone(),
    }
}

/// What the peer's refusal means for the caller; a link the peer no longer
/// knows is forgotten here, so the phone's reconciler links it afresh (§10.10).
async fn refusal(state: &AppState, peer: &Peer, error: PeerError) -> TaskError {
    let machine = peer.name.clone();
    match error {
        PeerError::Unreachable(_) | PeerError::NoAnswer(_) => {
            TaskError::MachineUnreachable(machine)
        }
        PeerError::Unauthorized => {
            unauthorized(state, &peer.peer_id).await;
            TaskError::PeerUnauthorized(machine)
        }
        PeerError::Refused { code, .. } if code == "delegations_disabled" => {
            TaskError::DelegationsDisabled(machine)
        }
        PeerError::Refused { code, .. } => TaskError::PeerRefused { machine, code },
    }
}

/// How `mirror` moves to match `remote`, if at all: never backwards, never
/// out of an end, and on a new question while waiting (a new line). The
/// peer's line is cut to a summary's length, as every local one is.
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
    let summary = remote
        .summary
        .as_deref()
        .map(|line| one_line(line, SUMMARY_CHARS))
        .filter(|line| !line.is_empty());
    let asks_anew = to == TaskState::Waiting && summary.is_some() && summary != mirror.summary;
    if to == mirror.state && !asks_anew {
        return None;
    }
    Some(TaskChange {
        to,
        source: SOURCE.into(),
        summary,
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
    moved(state, mirror, change).await
}

async fn moved(state: &AppState, mirror: &Task, change: TaskChange) -> bool {
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
/// a wait that doubles while it fails (or dies young); the mirrors read over
/// HTTP meanwhile. Ends when none is open, or when the peer no longer knows
/// this gateway.
async fn follow(state: AppState, peer_id: String) {
    let mut retry = RETRY_FIRST;
    loop {
        let Some(peer) = state
            .peers
            .get(&peer_id)
            .await
            .filter(|peer| !peer.unauthorized)
        else {
            return;
        };
        let Ok(client) = PeerClient::for_peer(&peer) else {
            return;
        };
        if open_mirrors(&state, &peer_id).await.is_empty() {
            return leave(&state, &peer_id).await;
        }
        let opened = Instant::now();
        match client.subscribe_tasks().await {
            Ok(mut feed) => {
                state.peers.mark_seen(&peer_id).await;
                match follow_feed(&state, &client, &peer, &mut feed).await {
                    Ended::Idle => return leave(&state, &peer_id).await,
                    Ended::Forgotten => return,
                    // The socket closing is the peer going away: unreachable
                    // now, not once its last answer is two minutes old.
                    Ended::Socket if opened.elapsed() >= IDLE_CHECK => {
                        state.peers.presence.down(&peer_id);
                        retry = RETRY_FIRST;
                        continue;
                    }
                    Ended::Socket => state.peers.presence.down(&peer_id),
                }
            }
            Err(PeerError::Unauthorized) => return unauthorized(&state, &peer_id).await,
            Err(_) => {
                state.peers.presence.down(&peer_id);
                if !read_all(&state, &client, &peer).await {
                    return;
                }
            }
        }
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(RETRY_MOST);
    }
}

/// No mirror of `peer_id` is open: give up the follower's place, unless one
/// was bound meanwhile -- then a new follower takes it.
async fn leave(state: &AppState, peer_id: &str) {
    state.peers.sockets.leave(peer_id);
    if !open_mirrors(state, peer_id).await.is_empty() {
        watch(state, peer_id);
    }
}

/// Why [`follow_feed`] returned.
enum Ended {
    /// The socket ended; worth opening again.
    Socket,
    /// No mirror of the peer is open.
    Idle,
    /// The peer no longer knows this gateway, and was forgotten.
    Forgotten,
}

/// Apply `feed` to the open mirrors, read afresh after every wait: a mirror
/// bound while the socket was quiet gets its frames too.
async fn follow_feed(
    state: &AppState,
    client: &PeerClient,
    peer: &Peer,
    feed: &mut TaskFeed,
) -> Ended {
    let mut open = open_mirrors(state, &peer.peer_id).await;
    loop {
        if open.is_empty() {
            return Ended::Idle;
        }
        let item = tokio::time::timeout(IDLE_CHECK, feed.next()).await;
        open = open_mirrors(state, &peer.peer_id).await;
        match item {
            // Quiet: seen only if B's own pings still arrive; and a mirror
            // whose create got no answer looks for its task (no frame may
            // ever name it).
            Err(_) => {
                if feed.heard().elapsed() < IDLE_CHECK {
                    state.peers.mark_seen(&peer.peer_id).await;
                }
                if open.iter().any(|mirror| remote_id(mirror).is_empty())
                    && !read_all(state, client, peer).await
                {
                    return Ended::Forgotten;
                }
            }
            Ok(None) => return Ended::Socket,
            Ok(Some(FeedItem::Task(remote))) => {
                state.peers.mark_seen(&peer.peer_id).await;
                let mirror = open
                    .iter()
                    .find(|mirror| remote_id(mirror) == remote.task_id);
                if let Some(mirror) = mirror {
                    apply(state, mirror, &remote).await;
                    continue;
                }
                // Perhaps the task of a mirror whose create got no answer.
                let unresolved = open.iter().any(|mirror| remote_id(mirror).is_empty());
                if unresolved && !read_all(state, client, peer).await {
                    return Ended::Forgotten;
                }
            }
            // First on every socket, and when B dropped frames.
            Ok(Some(FeedItem::Resync)) => {
                if !read_all(state, client, peer).await {
                    return Ended::Forgotten;
                }
            }
        }
    }
}

/// Read every open mirror of `peer` from it. `false`: the peer no longer
/// knows this gateway and was forgotten.
async fn read_all(state: &AppState, client: &PeerClient, peer: &Peer) -> bool {
    for mirror in open_mirrors(state, &peer.peer_id).await {
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
    let read = if remote_id(mirror).is_empty() {
        adopt(state, client, peer, mirror).await
    } else {
        match client.read_task(remote_id(mirror)).await {
            Ok(remote) => Ok(apply(state, mirror, &remote).await),
            Err(PeerError::Refused { status: 404, .. }) => {
                state
                    .tasks
                    .record_failure(mirror, ended(mirror, LOST_THERE))
                    .await;
                Ok(true)
            }
            Err(error) => Err(error),
        }
    };
    match read {
        Ok(moved) => {
            state.peers.mark_seen(&peer.peer_id).await;
            Some(moved)
        }
        Err(PeerError::Unauthorized) => {
            unauthorized(state, &peer.peer_id).await;
            None
        }
        // Quiet: the mirror keeps its state. Published anyway, so its view's
        // `peer_reachable`/`peer_last_seen_ms` (§10.12) reach the phone and
        // a blocked `tasks.wait` wakes to them.
        Err(_) => {
            state.peers.presence.down(&peer.peer_id);
            state.tasks.republish(mirror).await;
            Some(false)
        }
    }
}

/// Find the task over there that `mirror`'s unanswered create opened: the
/// one carrying the `client_ref` it sent. Bound to it once
/// it has started; failed as not confirmed when none appears within
/// [`ADOPT_WITHIN`].
async fn adopt(
    state: &AppState,
    client: &PeerClient,
    peer: &Peer,
    mirror: &Task,
) -> Result<bool, PeerError> {
    let theirs = client.own_tasks().await?;
    let found = theirs
        .into_iter()
        .find(|task| task.client_ref.is_some() && task.client_ref == mirror.client_ref);
    let Some(remote) = found else {
        if now_ms().saturating_sub(mirror.created_ms) < ADOPT_WITHIN.as_millis() as u64 {
            return Ok(false);
        }
        state
            .tasks
            .record_failure(mirror, ended(mirror, NOT_CONFIRMED))
            .await;
        return Ok(true);
    };
    // Bound with its first move; one still starting is found again next time.
    let Some(mut change) = mirror_change(mirror, &remote) else {
        return Ok(false);
    };
    change.binding = Some(binding(peer, remote.task_id.clone()));
    Ok(moved(state, mirror, change).await)
}

/// The peer answered 401/403: it was unlinked there, or this computer's
/// login revoked. Its open mirrors fail (reason `peer_unlinked`): the task over
/// there can never be read again, since a new link is a new login. Then the
/// peer is marked `unauthorized`, not forgotten: a delegation there says why,
/// and the phone's next link under its name replaces it -- last, since this
/// may run in its follower, which marking ends. Every path that finds a link
/// gone comes here.
pub(crate) async fn unauthorized(state: &AppState, peer_id: &str) {
    tracing::info!(peer = %peer_id, "a peer no longer takes this gateway's token");
    state.peers.presence.gone(peer_id);
    for mirror in open_mirrors(state, peer_id).await {
        let why = ended(&mirror, UNLINKED_THERE);
        state.tasks.record_failure(&mirror, why).await;
    }
    state.peers.mark_unauthorized(peer_id).await;
}

/// The open mirrors of `peer_id`.
async fn open_mirrors(state: &AppState, peer_id: &str) -> Vec<Task> {
    let filter = TaskFilter {
        states: Some(OPEN_STATES.to_vec()),
        peer: Some(peer_id.to_owned()),
        limit: 1000,
        ..Default::default()
    };
    state.tasks.open_tasks(filter).await.unwrap_or_default()
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
    let TaskBinding::Peer {
        peer_id,
        remote_task_id,
        ..
    } = &mirror.binding
    else {
        return false;
    };
    // A live socket follows a bound mirror; one whose create got no answer
    // is looked for here as well, so its deadline holds whatever the socket does.
    let live = state.peers.sockets.is_live(peer_id);
    if live && !remote_task_id.is_empty() {
        return false;
    }
    let Some(peer) = state
        .peers
        .get(peer_id)
        .await
        .filter(|peer| !peer.unauthorized)
    else {
        // Unlinked, or no longer taking this token (a restart between the
        // mark and its mirrors' end): nothing can read its task again.
        let why = ended(mirror, NO_LONGER_LINKED);
        state.tasks.record_failure(mirror, why).await;
        return true;
    };
    let Ok(client) = PeerClient::for_peer(&peer) else {
        return false;
    };
    let Some(moved) = refresh(state, &client, &peer, mirror).await else {
        return false;
    };
    if !live {
        watch(state, peer_id);
    }
    moved
}

/// Ask the peer to cancel the task `mirror` mirrors, best effort: the mirror
/// is cancelled here either way. One whose create got no answer is looked
/// up by its `client_ref` first, so a task that did start there stops too.
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
        let remote_task_id = match remote_task_id.as_str() {
            "" => {
                let theirs = client.own_tasks().await.unwrap_or_default();
                let started = theirs
                    .into_iter()
                    .find(|task| task.client_ref.is_some() && task.client_ref == mirror.client_ref);
                match started {
                    Some(task) => task.task_id,
                    None => return,
                }
            }
            known => known.to_owned(),
        };
        if let Err(error) = client.cancel_task(&remote_task_id).await {
            tracing::warn!(task = %mirror.id.0, peer = %peer.peer_id, %error, "the cancel did not reach the peer");
        }
    }
}

/// `peer_id` was unlinked here: its open mirrors fail (reason `peer_unlinked`).
pub(crate) async fn lose(state: &AppState, peer_id: &str) {
    for mirror in open_mirrors(state, peer_id).await {
        let lost = ended(&mirror, UNLINKED_HERE);
        state.tasks.record_failure(&mirror, lost).await;
    }
}

/// `mirror` fails with its computer's name and `ending` as its summary.
fn ended(mirror: &Task, ending: &str) -> TaskChange {
    let name = match &mirror.binding {
        TaskBinding::Peer { peer_name, .. } => peer_name.as_str(),
        _ => "",
    };
    failed(&format!("{name}{ending}"))
}

/// Why a mirror failed, or why an open one is stuck, for its view: one of
/// the summaries [`ended`] writes (matched whole, so a summary copied from
/// the task over there never counts), or `peer_unreachable` while its
/// computer has not answered lately. `None` for any other task.
pub(crate) fn reason(task: &Task, reachable: Option<bool>) -> Option<&'static str> {
    let TaskBinding::Peer { peer_name, .. } = &task.binding else {
        return None;
    };
    if task.state == TaskState::Failed {
        let rest = task.summary.as_deref()?.strip_prefix(peer_name.as_str())?;
        return ENDINGS
            .iter()
            .find(|(ending, _)| *ending == rest)
            .map(|(_, reason)| *reason);
    }
    (!task.state.is_terminal() && reachable == Some(false)).then_some("peer_unreachable")
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
                peer_server_id: "srv-mini".into(),
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
            mentions: Vec::new(),
            client_ref: None,
            requester_server_id: None,
        }
    }

    fn remote(state: TaskState, summary: Option<&str>) -> RemoteTask {
        RemoteTask {
            task_id: "t_r".into(),
            client_ref: None,
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
            unauthorized: false,
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
        // The peer's line is cut to a summary's length, as a local one is.
        let long = "x".repeat(10_000);
        let change = mirror_change(&mirror(Working, None), &remote(Waiting, Some(&long))).unwrap();
        assert!(change.summary.unwrap().chars().count() <= SUMMARY_CHARS);
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
        assert_eq!(view["binding"]["peer_server_id"], "srv-mini");
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

        // A mirror bound while the socket waits gets its frames too: the
        // short goal ends with no frame between its start and its end.
        let view = delegate_as(&coding, "mini", "long goal").await.unwrap();
        let (long_mirror, long_remote) = ids(&view);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let view = delegate_as(&coding, "mini", "short goal").await.unwrap();
        let (short_mirror, short_remote) = ids(&view);
        mini.tasks
            .complete(&short_remote, "short done", Outcome::Done)
            .await
            .unwrap();
        let by = owner(&coding, &short_mirror).await;
        let waited = coding
            .tasks
            .wait(&coding, &by, &short_mirror, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            waited.task.state,
            TaskState::Done,
            "the second mirror's frame"
        );
        mini.tasks
            .complete(&long_remote, "long done", Outcome::Done)
            .await
            .unwrap();
        let by = owner(&coding, &long_mirror).await;
        let waited = coding
            .tasks
            .wait(&coding, &by, &long_mirror, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            waited.task.state,
            TaskState::Done,
            "the first mirror's frame"
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
        let unlinked = coding.peers.unlink(&coding, "srv-mini").await.unwrap();
        assert!(unlinked.forgotten);
        let lost = coding.tasks.get_for_test(&mirror_id).await;
        assert_eq!(lost.state, TaskState::Failed);
        assert_eq!(lost.summary.as_deref(), Some("mini was unlinked"));
        let view = coding.tasks.present(&lost).await;
        assert_eq!(view.reason, Some("peer_unlinked"), "the phone words it");
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
        // A mirror still open there when it turns out revoked fails at once.
        let stranded = coding
            .tasks
            .insert_for_test(&Task {
                binding: TaskBinding::Peer {
                    peer_id: "srv-old".into(),
                    peer_server_id: "srv-old".into(),
                    remote_task_id: "task-there".into(),
                    peer_name: "old".into(),
                },
                ..mirror(TaskState::Working, None)
            })
            .await;
        // Its own login here may still be good: forgetting leaves it.
        coding
            .devices
            .lock()
            .unwrap()
            .push(crate::connectivity::authority::DeviceRecord {
                server_id: Some("srv-old".into()),
                ..test_peer_device("old-login", "old-token")
            });
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
        // A revoked link is kept, marked: the next delegation there still
        // names the reason, never machine_unknown, and the phone sees it.
        let old = coding.peers.get("srv-old").await.unwrap();
        assert!(old.unauthorized && old.view(now_ms()).unauthorized);
        let again = delegate_as(&coding, "old", "goal").await.unwrap_err();
        assert_eq!(again["error"], "unauthorized", "{again}");
        let stranded = coding.tasks.get_for_test(&stranded).await;
        assert_eq!(
            (stranded.state, stranded.summary.as_deref()),
            (
                TaskState::Failed,
                Some("old no longer accepts this computer")
            )
        );
        assert!(
            coding
                .devices
                .lock()
                .unwrap()
                .iter()
                .any(|d| d.id == "old-login"),
            "a forgotten peer keeps its login here"
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

        // One hop only: an agent working on a task mini handed here cannot
        // hand work to a linked computer in turn.
        coding
            .tasks
            .insert_for_test(&Task {
                origin: TaskOrigin::Peer,
                binding: TaskBinding::Pane {
                    session_id: "default".into(),
                    pane_id: "%1".into(),
                    tab_id: None,
                    shell_pid: None,
                    agent_id: None,
                },
                ..mirror(TaskState::Working, None)
            })
            .await;
        let error = delegate_as(&coding, "mini", "goal").await.unwrap_err();
        assert_eq!(error["error"], "delegation_loop", "{error}");
    }

    /// A task is not done while a child it delegated is still out -- working,
    /// paused or waiting: `tasks.complete` names it; blocked still goes.
    #[tokio::test]
    async fn a_parent_is_not_done_while_its_child_is_still_out() {
        let state = crate::test_support::test_state("admin", vec![]);
        let parent = state
            .tasks
            .insert_for_test(&Task {
                binding: TaskBinding::None,
                ..mirror(TaskState::Working, None)
            })
            .await;
        let child = state
            .tasks
            .insert_for_test(&Task {
                parent: Some(parent.clone()),
                goal: "Check the Xcode version".into(),
                ..mirror(TaskState::IdleUnconfirmed, None)
            })
            .await;
        let refused = state
            .tasks
            .complete(&parent, "all done", Outcome::Done)
            .await
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "mini is still on 'Check the Xcode version'; wait for it or cancel it."
        );
        let by = owner(&state, &child).await;
        state.tasks.cancel(&state, &by, &child).await.unwrap();
        assert_eq!(
            state
                .tasks
                .complete(&parent, "all done", Outcome::Done)
                .await
                .unwrap()
                .state,
            TaskState::Done
        );
    }

    /// What a mirror says about its computer (§10.12): structured fields,
    /// never a gateway-made line; the phone words them.
    #[tokio::test]
    async fn a_mirror_says_whether_its_computer_answers() {
        let state = crate::test_support::test_state("admin", vec![]);
        let now = now_ms();
        for (seen, down, reachable) in [
            (Some(now), false, true),
            (Some(now - 5 * 60_000), false, false),
            (None, false, false),
            // Its task socket just closed: unreachable at once, its last
            // answer kept.
            (Some(now), true, false),
        ] {
            state
                .peers
                .store_for_test(peer("srv-mini", "mini", "http://127.0.0.1:9", seen))
                .await;
            if down {
                state.peers.presence.down("srv-mini");
            }
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
            let stuck = (!reachable).then_some("peer_unreachable");
            assert_eq!(view.reason, stuck, "{seen:?}");
            assert_eq!(view.progress, None, "no gateway-made progress line");
        }
    }

    /// A create that got no answer, through a live follower: the mirror finds
    /// its task over there by the `client_ref` it sent, never by its goal;
    /// one not found fails as not confirmed once the deadline passes, socket
    /// or not.
    #[tokio::test]
    async fn a_mirror_whose_create_got_no_answer_finds_its_task_there() {
        let herdr = FakeHerdr::start(vec!["$ "], None);
        let mini = named(output_state(&herdr), "srv-mini", "mini");
        mini.devices
            .lock()
            .unwrap()
            .push(test_peer_device("coding-1", "peer-token"));
        let url = serve(peer_router(mini.clone())).await;
        let now = now_ms();
        let mut there = Vec::new();
        for (goal, client_ref, created_ms) in [
            ("same goal", Some("ref-1"), now),
            ("older run", None, now - 10 * 60_000),
            ("cancelled early", Some("ref-4"), now),
        ] {
            let task = Task {
                state: TaskState::Working,
                binding: TaskBinding::None,
                created_by: TaskPrincipal {
                    kind: PrincipalKind::Gateway,
                    id: "coding-1".into(),
                },
                origin: TaskOrigin::Peer,
                goal: goal.into(),
                created_ms,
                client_ref: client_ref.map(Into::into),
                ..mirror(TaskState::Working, None)
            };
            there.push(mini.tasks.insert_for_test(&task).await.0);
        }
        let coding = named(output_state(&herdr), "srv-coding", "coding");
        coding
            .peers
            .store_for_test(peer("srv-mini", "mini", &url, Some(now)))
            .await;
        let almost_due = now - ADOPT_WITHIN.as_millis() as u64 + 600;
        let mut rows = Vec::new();
        for (goal, client_ref, created_ms, state, bound) in [
            (
                "same goal",
                "ref-1",
                now,
                TaskState::Working,
                Some(&there[0]),
            ),
            ("older run", "ref-3", almost_due, TaskState::Failed, None),
        ] {
            let id = coding
                .tasks
                .insert_for_test(&Task {
                    goal: goal.into(),
                    created_ms,
                    client_ref: Some(client_ref.into()),
                    binding: TaskBinding::Peer {
                        peer_id: "srv-mini".into(),
                        peer_server_id: "srv-mini".into(),
                        remote_task_id: String::new(),
                        peer_name: "mini".into(),
                    },
                    ..mirror(TaskState::Dispatching, None)
                })
                .await;
            rows.push((goal, id, state, bound));
        }
        watch(&coding, "srv-mini");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for (goal, id, state, bound) in rows {
            let now = loop {
                let now = coding.tasks.get_for_test(&id).await;
                if now.state == state || tokio::time::Instant::now() > deadline {
                    break now;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            assert_eq!(now.state, state, "{goal}");
            match bound {
                Some(bound) => assert_eq!(remote_id(&now), bound.as_str(), "{goal}"),
                None => assert_eq!(
                    now.summary.as_deref(),
                    Some("mini did not confirm the task"),
                    "{goal}"
                ),
            }
        }
        assert!(
            coding.peers.sockets.is_live("srv-mini"),
            "found through the live follower"
        );

        // Cancelled before its task was found: the task it did start there stops too.
        let unresolved = Task {
            goal: "cancelled early".into(),
            client_ref: Some("ref-4".into()),
            binding: TaskBinding::Peer {
                peer_id: "srv-mini".into(),
                peer_server_id: "srv-mini".into(),
                remote_task_id: String::new(),
                peer_name: "mini".into(),
            },
            ..mirror(TaskState::Dispatching, None)
        };
        forward_cancel(&coding, &unresolved).await;
        assert_eq!(
            mini.tasks
                .get_for_test(&TaskId(there[2].clone()))
                .await
                .state,
            TaskState::Cancelled
        );
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
