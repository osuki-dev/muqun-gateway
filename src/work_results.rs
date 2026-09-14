//! Result capture and commit used by paired humans and scoped local assistants.
use crate::work::model::*;
use crate::work_authority::{Registry, Scope};
use std::sync::{Arc, Mutex};
#[derive(Clone)]
pub(super) struct Actor {
    pub id: String,
    pub local: Option<(Arc<Mutex<Registry>>, String, Scope)>,
}
fn authorized<T>(actor: &Actor, action: impl FnOnce() -> WorkResult<T>) -> WorkResult<T> {
    if let Some((registry, token, scope)) = &actor.local {
        let guard = registry
            .lock()
            .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        guard.authorize(token, scope, now())?;
        action()
    } else {
        action()
    }
}
pub(super) async fn submit(
    state: &crate::AppState,
    actor: Actor,
    session: String,
    task_id: String,
    key: String,
    revision: u64,
    input: ResultInput,
) -> WorkResult<Mutation<ResultSubmission>> {
    if let Some((_, _, scope)) = &actor.local {
        if scope.session_id != session
            || scope.task_id != task_id
            || scope.attempt_id != input.attempt_id
        {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
    }
    let store = state
        .work
        .clone()
        .ok_or(WorkError(FailureCode::StorageUnavailable))?;
    let first_store = store.clone();
    let first_actor = actor.clone();
    let sid = session.clone();
    let tid = task_id.clone();
    let request_key = key.clone();
    let request = input.clone();
    let (replay, repo) = tokio::task::spawn_blocking(move || {
        authorized(&first_actor, || {
            let mut store = first_store
                .lock()
                .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
            if let Some(result) = store.replay_result(
                &first_actor.id,
                &sid,
                &tid,
                &request_key,
                revision,
                &request,
            )? {
                return Ok((Some(result), None));
            }
            let detail = store.detail(&sid, &tid)?;
            if detail.task.revision != revision {
                return Err(WorkError(FailureCode::RevisionConflict));
            }
            let attempt = detail
                .attempts
                .iter()
                .find(|a| a.id == request.attempt_id)
                .ok_or(WorkError(FailureCode::ScopeMismatch))?;
            let root = attempt
                .binding
                .worktree_path
                .clone()
                .unwrap_or(detail.task.repo_path);
            Ok((None, Some(std::path::PathBuf::from(root))))
        })
    })
    .await
    .map_err(|_| WorkError(FailureCode::StorageUnavailable))??;
    if let Some(replay) = replay {
        return Ok(replay);
    }
    if !input.artifacts.is_empty() {
        let root = state
            .work_artifacts
            .clone()
            .ok_or(WorkError(FailureCode::StorageUnavailable))?;
        let repo = repo.ok_or(WorkError(FailureCode::StorageUnavailable))?;
        let artifacts = input.artifacts.clone();
        tokio::task::spawn_blocking(move || {
            crate::work_artifacts::capture(&repo, &root, &artifacts)
        })
        .await
        .map_err(|_| WorkError(FailureCode::StorageUnavailable))??;
    }
    // Revocation and the final commit share this authorization guard. No native I/O occurs under it.
    tokio::task::spawn_blocking(move || {
        authorized(&actor, || {
            let mut store = store
                .lock()
                .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
            store.submit_result(&actor.id, &session, &task_id, &key, revision, input, now())
        })
    })
    .await
    .map_err(|_| WorkError(FailureCode::StorageUnavailable))?
}
fn now() -> i64 {
    crate::now_unix_ms().min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_authorization_guard_serializes_revocation_with_commit() {
        let dir =
            std::env::temp_dir().join(format!("muqun-result-authority-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let registry = Arc::new(Mutex::new(Registry::default()));
        let scope = Scope {
            session_id: "s".into(),
            task_id: "t".into(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
        };
        let path = registry
            .lock()
            .unwrap()
            .issue(&dir, &dir.join("socket"), scope.clone(), now())
            .unwrap();
        let context: crate::work_authority::ContextFile =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let actor = Actor {
            id: "attempt:a".into(),
            local: Some((registry.clone(), context.token, scope.clone())),
        };
        authorized(&actor, || {
            assert!(
                registry.try_lock().is_err(),
                "revocation cannot interleave inside commit authorization"
            );
            Ok(())
        })
        .unwrap();
        registry.lock().unwrap().revoke(&scope);
        let mut executed = false;
        assert!(authorized(&actor, || {
            executed = true;
            Ok(())
        })
        .is_err());
        assert!(!executed);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
