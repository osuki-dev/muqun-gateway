//! Narrow local application authority. Same-UID processes are not an OS sandbox.
use crate::work::model::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_GRANTS: usize = 256;
const MAX_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Scope {
    pub session_id: String,
    pub task_id: String,
    pub attempt_id: String,
}
#[derive(Clone)]
struct Grant {
    scope: Scope,
    hash: String,
    expires_at_ms: i64,
    context_file: PathBuf,
    delegation: Option<DelegationFence>,
}
/// Cannot be constructed from wire input; valid only while the registry guard is retained.
pub(super) struct ActivationPermit {
    grant_id: String,
    scope: Scope,
}
#[derive(Clone)]
pub(super) struct DelegationAuthority {
    pub scope: Scope,
    pub fence: DelegationFence,
    pub actor_id: String,
}
#[derive(Default)]
pub(super) struct Registry {
    grants: HashMap<String, Grant>,
}
#[derive(Serialize, Deserialize)]
pub(super) struct ContextFile {
    pub socket: PathBuf,
    pub token: String,
    pub scope: Scope,
    pub expires_at_ms: i64,
}
fn denied() -> WorkError {
    WorkError(FailureCode::ScopeMismatch)
}
impl Registry {
    pub fn issue(
        &mut self,
        dir: &Path,
        socket: &Path,
        scope: Scope,
        now: i64,
    ) -> WorkResult<PathBuf> {
        let expired: Vec<String> = self
            .grants
            .iter()
            .filter(|(_, g)| g.expires_at_ms <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.revoke_id(&id);
        }
        if self.grants.len() >= MAX_GRANTS {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        if self.grants.values().any(|g| g.scope == scope) {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        // UUID's version/variant bytes are not uniformly random; exclude them.
        let token: String = (0..3)
            .flat_map(|_| {
                uuid::Uuid::new_v4()
                    .into_bytes()
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, b)| (!matches!(i, 6 | 8)).then_some(b))
            })
            .take(32)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let id = uuid::Uuid::new_v4().to_string();
        uuid::Uuid::parse_str(&scope.attempt_id)
            .map_err(|_| WorkError(FailureCode::InvalidInput))?;
        let path = dir.join(format!("{}.json", scope.attempt_id));
        let expires_at_ms = now.saturating_add(MAX_LIFETIME_MS);
        let context = ContextFile {
            socket: socket.to_owned(),
            token: token.clone(),
            scope: scope.clone(),
            expires_at_ms,
        };
        write_context(&path, &context)?;
        self.grants.insert(
            id,
            Grant {
                scope,
                hash: crate::authority::hash_token(&token),
                expires_at_ms,
                context_file: path.clone(),
                delegation: None,
            },
        );
        Ok(path)
    }
    pub fn identify(&self, token: &str, now: i64) -> WorkResult<Scope> {
        if token.len() != 64 {
            return Err(denied());
        }
        let presented = crate::authority::hash_token(token);
        let mut scope = None;
        for grant in self.grants.values() {
            let equal = grant
                .hash
                .bytes()
                .zip(presented.bytes())
                .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                == 0;
            if equal && grant.expires_at_ms > now {
                scope = Some(grant.scope.clone());
            }
        }
        scope.ok_or_else(denied)
    }
    pub fn authorize(&self, token: &str, scope: &Scope, now: i64) -> WorkResult<()> {
        if self.identify(token, now)? != *scope {
            return Err(denied());
        }
        Ok(())
    }
    pub fn prepare_activation(&self, scope: &Scope, now: i64) -> WorkResult<ActivationPermit> {
        self.grants
            .iter()
            .find(|(_, grant)| grant.scope == *scope && grant.expires_at_ms > now)
            .map(|(id, _)| ActivationPermit {
                grant_id: id.clone(),
                scope: scope.clone(),
            })
            .ok_or_else(denied)
    }
    /// Paired configuration has already committed under the same registry guard.
    /// A missing/mismatched permit leaves runtime authority disabled, never recreates a grant.
    pub fn apply_activation(&mut self, permit: ActivationPermit, fence: DelegationFence) {
        if permit.scope.task_id != fence.coordinator_task_id
            || permit.scope.attempt_id != fence.coordinator_attempt_id
        {
            return;
        }
        if !self
            .grants
            .get(&permit.grant_id)
            .is_some_and(|grant| grant.scope == permit.scope)
        {
            return;
        }
        if self
            .grants
            .values()
            .filter(|grant| {
                grant.scope.session_id == permit.scope.session_id
                    && grant.scope.task_id == permit.scope.task_id
            })
            .filter_map(|grant| grant.delegation.as_ref())
            .any(|active| active.coordinator_epoch > fence.coordinator_epoch)
        {
            return;
        }
        self.clear_delegation(&permit.scope.session_id, &permit.scope.task_id);
        if let Some(grant) = self.grants.get_mut(&permit.grant_id) {
            grant.delegation = Some(fence);
        }
    }
    /// Invalidate only the observed controller generation; delayed exit evidence cannot revoke a successor.
    pub fn invalidate_delegation(&mut self, scope: &Scope, fence: &DelegationFence) -> bool {
        let mut changed = false;
        for grant in self.grants.values_mut() {
            if &grant.scope == scope && grant.delegation.as_ref() == Some(fence) {
                grant.delegation = None;
                changed = true;
            }
        }
        changed
    }
    pub fn clear_delegation(&mut self, session: &str, task_id: &str) {
        for grant in self
            .grants
            .values_mut()
            .filter(|grant| grant.scope.session_id == session && grant.scope.task_id == task_id)
        {
            grant.delegation = None;
        }
    }
    pub fn authorize_delegation(&self, token: &str, now: i64) -> WorkResult<DelegationAuthority> {
        let scope = self.identify(token, now)?;
        let fence = self
            .grants
            .values()
            .find(|grant| grant.scope == scope && grant.expires_at_ms > now)
            .and_then(|grant| grant.delegation.clone())
            .ok_or_else(denied)?;
        if fence.coordinator_task_id != scope.task_id
            || fence.coordinator_attempt_id != scope.attempt_id
        {
            return Err(denied());
        }
        let actor_id = format!(
            "delegation:{}:{}",
            scope.attempt_id, fence.coordinator_epoch
        );
        Ok(DelegationAuthority {
            scope,
            fence,
            actor_id,
        })
    }
    pub fn revoke(&mut self, scope: &Scope) -> bool {
        let paths = self.revoke_memory(scope);
        let found = !paths.is_empty();
        cleanup_context_files(paths);
        found
    }
    /// Called under the grant guard after a successful release transaction.
    /// Filesystem cleanup is deliberately deferred until database/grant locks end.
    pub fn revoke_memory(&mut self, scope: &Scope) -> Vec<PathBuf> {
        let ids: Vec<String> = self
            .grants
            .iter()
            .filter(|(_, g)| g.scope == *scope)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.grants.remove(&id).map(|grant| grant.context_file))
            .collect()
    }
    fn revoke_id(&mut self, id: &str) {
        if let Some(grant) = self.grants.remove(id) {
            let _ = std::fs::remove_file(grant.context_file);
        }
    }
}
pub(super) fn cleanup_context_files(paths: Vec<PathBuf>) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}
#[cfg(unix)]
fn write_context(path: &Path, context: &ContextFile) -> WorkResult<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
    let written = file
        .write_all(&serde_json::to_vec(context)?)
        .and_then(|_| file.sync_all());
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written.map_err(|_| WorkError(FailureCode::StorageUnavailable))
}
#[cfg(not(unix))]
fn write_context(_path: &Path, _context: &ContextFile) -> WorkResult<()> {
    Err(WorkError(FailureCode::CapabilityUnavailable))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delegation_activation_is_private_epoch_bound_and_preserves_reporting() {
        let dir =
            std::env::temp_dir().join(format!("muqun-delegation-grants-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut registry = Registry::default();
        let root = Scope {
            session_id: "s".into(),
            task_id: "root".into(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
        };
        let worker = Scope {
            session_id: "s".into(),
            task_id: "child".into(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
        };
        let root_file = registry
            .issue(&dir, &dir.join("socket"), root.clone(), 1)
            .unwrap();
        let worker_file = registry
            .issue(&dir, &dir.join("socket"), worker.clone(), 1)
            .unwrap();
        let root_context: ContextFile =
            serde_json::from_slice(&std::fs::read(root_file).unwrap()).unwrap();
        let worker_context: ContextFile =
            serde_json::from_slice(&std::fs::read(worker_file).unwrap()).unwrap();
        assert!(registry
            .authorize_delegation(&root_context.token, 2)
            .is_err());
        let mut fence = DelegationFence {
            coordinator_task_id: root.task_id.clone(),
            coordinator_attempt_id: root.attempt_id.clone(),
            coordinator_epoch: 1,
            instance_id: "launch".into(),
            native_owner_epoch: "owner".into(),
        };
        let permit = registry.prepare_activation(&root, 2).unwrap();
        registry.apply_activation(permit, fence.clone());
        let authority = registry
            .authorize_delegation(&root_context.token, 3)
            .unwrap();
        assert_eq!(authority.scope, root);
        assert_eq!(authority.fence, fence);
        assert_eq!(
            authority.actor_id,
            format!("delegation:{}:1", root.attempt_id)
        );
        assert!(registry
            .authorize_delegation(&worker_context.token, 3)
            .is_err());
        let stale = registry.prepare_activation(&root, 3).unwrap();
        let fresh = registry.prepare_activation(&root, 3).unwrap();
        fence.coordinator_epoch = 2;
        registry.apply_activation(fresh, fence.clone());
        let mut older = fence.clone();
        older.coordinator_epoch = 1;
        registry.apply_activation(stale, older.clone());
        assert!(!registry.invalidate_delegation(&root, &older));
        assert_eq!(
            registry
                .authorize_delegation(&root_context.token, 4)
                .unwrap()
                .fence
                .coordinator_epoch,
            2
        );
        assert!(registry.invalidate_delegation(&root, &fence));
        assert!(!registry.invalidate_delegation(&root, &fence));
        assert!(registry
            .authorize_delegation(&root_context.token, 4)
            .is_err());
        assert!(registry.authorize(&root_context.token, &root, 4).is_ok());
        assert!(registry
            .authorize(&worker_context.token, &worker, 4)
            .is_ok());
        let permit = registry.prepare_activation(&root, 4).unwrap();
        registry.revoke(&root);
        registry.apply_activation(permit, fence);
        assert!(registry
            .authorize_delegation(&root_context.token, 5)
            .is_err());
        assert!(registry
            .authorize(&worker_context.token, &worker, 5)
            .is_ok());
        assert!(registry
            .prepare_activation(&worker, MAX_LIFETIME_MS + 1)
            .is_err());
        drop(registry);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn scope_expiry_revoke_and_restart_are_enforced() {
        let dir = std::env::temp_dir().join(format!("muqun-grants-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut registry = Registry::default();
        let scope = Scope {
            session_id: "s".into(),
            task_id: "t".into(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
        };
        let path = registry
            .issue(&dir, &dir.join("socket"), scope.clone(), 1)
            .unwrap();
        let context: ContextFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(registry.authorize(&context.token, &scope, 2).is_ok());
        let mut other = scope.clone();
        other.attempt_id = "other".into();
        assert!(registry.authorize(&context.token, &other, 2).is_err());
        assert!(registry
            .authorize(&context.token, &scope, MAX_LIFETIME_MS + 1)
            .is_err());
        assert!(Registry::default().identify(&context.token, 2).is_err());
        assert!(registry.revoke(&scope));
        assert!(!path.exists());
        assert!(registry.identify(&context.token, 2).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
