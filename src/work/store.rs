use super::model::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

mod delegation;
mod inputs;
mod interruptions;
pub mod pagination;
mod summaries;

pub struct WorkStore {
    connection: Connection,
    summaries_ready: bool,
}
fn reservation_active(attempt: &Attempt) -> bool {
    attempt.lifecycle.reservation != Reservation::Released
}
fn native_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
#[derive(serde::Serialize, serde::Deserialize)]
struct ReconciliationIntent {
    operation_id: String,
    input: ReconcileInput,
}
fn record_by_operation<T: DeserializeOwned>(
    db: &Connection,
    session: &str,
    kind: &str,
    operation_id: &str,
) -> WorkResult<Option<T>> {
    let body: Option<String> = db.query_row("SELECT body FROM records WHERE session=?1 AND kind=?2 AND json_extract(body,'$.operation_id')=?3", params![session,kind,operation_id], |r|r.get(0)).optional()?;
    body.map(|json| serde_json::from_str(&json).map_err(Into::into))
        .transpose()
}
/// Reserve quota before admission; final facts replace this row instead of allocating.
fn reserve_fact(db: &Connection, session: &str, op: &Operation, kind: &str) -> WorkResult<()> {
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE session=?1 AND kind=?2 AND json_extract(body,'$.operation_id')=?3)", params![session,kind,op.id], |r|r.get(0))?;
    if !exists {
        put(
            db,
            session,
            kind,
            &id(),
            &op.task_id,
            &serde_json::json!({"operation_id":op.id}),
        )?;
    }
    Ok(())
}
fn fulfill_fact(
    db: &Connection,
    session: &str,
    op: &Operation,
    reserved_kind: &str,
    kind: &str,
    value: &impl Serialize,
) -> WorkResult<()> {
    let updated = db.execute("UPDATE records SET kind=?4, body=?5 WHERE session=?1 AND kind=?2 AND task_id=?6 AND json_extract(body,'$.operation_id')=?3", params![session,reserved_kind,op.id,kind,encoded(value)?,op.task_id])?;
    if updated != 1 {
        return Err(WorkError(FailureCode::StorageUnavailable));
    }
    Ok(())
}
fn reconciliation(
    db: &Connection,
    session: &str,
    operation_id: &str,
) -> WorkResult<Option<ReconciliationReceipt>> {
    record_by_operation(db, session, "reconciliation", operation_id)
}
fn start_refusal(
    db: &Connection,
    session: &str,
    operation_id: &str,
) -> WorkResult<Option<NativeStartRefusal>> {
    let body: Option<String> = db.query_row("SELECT body FROM records WHERE session=?1 AND kind='start_refusal' AND json_extract(body,'$.start_operation_id')=?2", params![session,operation_id], |r|r.get(0)).optional()?;
    body.map(|json| serde_json::from_str(&json).map_err(Into::into))
        .transpose()
}
fn start_attempt(db: &Connection, session: &str, op: &Operation) -> WorkResult<Attempt> {
    if op.kind != OperationKind::StartAttempt {
        return Err(invalid());
    }
    let attempt: Attempt = load(
        db,
        session,
        "attempt",
        op.attempt_id.as_deref().ok_or_else(invalid)?,
    )?;
    if attempt.task_id != op.task_id {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    Ok(attempt)
}
fn reconcile_binding(attempt: &Attempt, task_id: &str, input: &ReconcileInput) -> WorkResult<()> {
    if attempt.task_id != task_id {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    if attempt.binding.instance_id != input.expected_instance_id
        || attempt.lifecycle.native_owner_epoch != input.expected_native_owner_epoch
    {
        return Err(WorkError(FailureCode::InstanceChanged));
    }
    Ok(())
}
fn match_native_binding(attempt: &Attempt, instance_id: &str, owner_epoch: &str) -> WorkResult<()> {
    if !native_identity(instance_id) || !native_identity(owner_epoch) {
        return Err(invalid());
    }
    if attempt.lifecycle.launch_phase != LaunchPhase::LaunchConfirmed
        || attempt.binding.instance_id.as_deref() != Some(instance_id)
        || attempt.lifecycle.native_owner_epoch.as_deref() != Some(owner_epoch)
    {
        return Err(WorkError(FailureCode::InstanceChanged));
    }
    Ok(())
}
fn invalid() -> WorkError {
    WorkError(FailureCode::InvalidInput)
}
fn bounded(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.len() <= max && !s.contains('\0')
}
pub fn payload_digest(value: &impl Serialize) -> WorkResult<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn encoded(value: &impl Serialize) -> WorkResult<String> {
    Ok(serde_json::to_string(value)?)
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn load<T: DeserializeOwned>(
    db: &Connection,
    session: &str,
    kind: &str,
    id: &str,
) -> WorkResult<T> {
    let json: Option<String> = db
        .query_row(
            "SELECT body FROM records WHERE session=?1 AND kind=?2 AND id=?3",
            params![session, kind, id],
            |r| r.get(0),
        )
        .optional()?;
    serde_json::from_str(&json.ok_or(WorkError(FailureCode::NotFound))?).map_err(Into::into)
}
fn put(
    db: &Connection,
    session: &str,
    kind: &str,
    id: &str,
    task: &str,
    value: &impl Serialize,
) -> WorkResult<()> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM records WHERE id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if !exists {
        let count: u64 = db.query_row(
            "SELECT COUNT(*) FROM records WHERE session=?1 AND task_id=?2",
            params![session, task],
            |r| r.get(0),
        )?;
        if count >= 1024 {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
    }
    db.execute("INSERT INTO records(session,kind,id,task_id,body) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET body=excluded.body",params![session,kind,id,task,encoded(value)?])?;
    Ok(())
}
fn records<T: DeserializeOwned>(
    db: &Connection,
    session: &str,
    kind: &str,
    task: &str,
) -> WorkResult<Vec<T>> {
    let mut stmt = db.prepare(
        "SELECT body FROM records WHERE session=?1 AND kind=?2 AND task_id=?3 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![session, kind, task], |r| r.get::<_, String>(0))?;
    rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
}
fn cursor(db: &Connection) -> WorkResult<u64> {
    Ok(
        db.query_row("SELECT COALESCE(MAX(cursor),0) FROM changes", [], |r| {
            r.get(0)
        })?,
    )
}
fn changed(db: &Connection, task: &mut Task, kind: &str, entity: &str, now: i64) -> WorkResult<()> {
    task.revision += 1;
    task.updated_at_ms = now;
    put(db, &task.session_id, "task", &task.id, &task.id, task)?;
    db.execute(
        "INSERT INTO changes(session,task_id,revision,kind,entity_id) VALUES(?1,?2,?3,?4,?5)",
        params![task.session_id, task.id, task.revision, kind, entity],
    )?;
    summaries::on_change(
        db,
        &task.session_id,
        &task.id,
        db.last_insert_rowid(),
        kind,
        entity,
    )?;
    db.execute(
        "DELETE FROM changes WHERE cursor <= (SELECT MAX(cursor)-10000 FROM changes)",
        [],
    )?;
    Ok(())
}
fn revision(task: &Task, expected: u64) -> WorkResult<()> {
    if task.revision != expected {
        return Err(WorkError(FailureCode::RevisionConflict));
    }
    Ok(())
}

/// Pausing a parent fences future launches throughout its delegation tree.
/// Existing input and explicit result review remain available.
fn launch_allowed(db: &Connection, task: &Task) -> WorkResult<()> {
    let mut current = task.clone();
    let mut visited = std::collections::HashSet::new();
    loop {
        if current.paused {
            return Err(WorkError(FailureCode::NotReady));
        }
        if !visited.insert(current.id.clone()) || visited.len() > 64 {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        match current.parent_task_id {
            Some(ref parent) => current = load(db, &task.session_id, "task", parent)?,
            None => return Ok(()),
        }
    }
}
/// Every descendant attempt, including a child lead, consumes its ancestors' allowance.
/// The current task's own lead is the coordinator and is the only excluded attempt.
fn delegated_reservations(db: &Connection, session: &str, ancestor: &str) -> WorkResult<u64> {
    let mut statement = db.prepare(
        "WITH RECURSIVE tree(id) AS (
            SELECT ?2 UNION ALL
            SELECT r.id FROM records r JOIN tree ON json_extract(r.body,'$.parent_task_id')=tree.id WHERE r.session=?1 AND r.kind='task'
        )
        SELECT a.body FROM records a JOIN tree ON tree.id=a.task_id
        WHERE a.session=?1 AND a.kind='attempt'
          AND (a.task_id!=?2 OR json_extract(a.body,'$.role')='worker')"
    )?;
    let rows = statement.query_map(params![session, ancestor], |r| r.get::<_, String>(0))?;
    let mut count = 0;
    for row in rows {
        let attempt: Attempt = serde_json::from_str(&row?)?;
        count += u64::from(reservation_active(&attempt));
    }
    Ok(count)
}
fn delegation_budget(db: &Connection, task: &Task, role: &AttemptRole) -> WorkResult<()> {
    let mut scope = if *role == AttemptRole::Worker {
        Some(task.clone())
    } else {
        task.parent_task_id
            .as_ref()
            .map(|id| load::<Task>(db, &task.session_id, "task", id))
            .transpose()?
    };
    let mut depth = 0;
    while let Some(ancestor) = scope {
        depth += 1;
        if depth > 16 {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        if delegated_reservations(db, &task.session_id, &ancestor.id)?
            >= u64::from(ancestor.policy.max_workers)
        {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        scope = ancestor
            .parent_task_id
            .as_ref()
            .map(|id| load::<Task>(db, &task.session_id, "task", id))
            .transpose()?;
    }
    Ok(())
}
struct Request<'a> {
    actor: &'a str,
    session: &'a str,
    key: &'a str,
    kind: &'a str,
    digest: String,
}
impl Request<'_> {
    fn replay<T: DeserializeOwned>(&self, db: &Connection) -> WorkResult<Option<T>> {
        if !bounded(self.actor, 256) || !bounded(self.session, 256) || !bounded(self.key, 128) {
            return Err(invalid());
        }
        let row: Option<(String,String)>=db.query_row("SELECT digest,body FROM receipts WHERE actor=?1 AND session=?2 AND kind=?3 AND request_key=?4",params![self.actor,self.session,self.kind,self.key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        match row {
            None => {
                let count: u64 = db.query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))?;
                if count >= 100000 {
                    return Err(WorkError(FailureCode::ResourceLimit));
                }
                Ok(None)
            }
            Some((digest, body)) => {
                if digest != self.digest {
                    return Err(WorkError(FailureCode::RequestKeyConflict));
                }
                Ok(Some(serde_json::from_str(&body)?))
            }
        }
    }
    fn save(&self, db: &Connection, value: &impl Serialize) -> WorkResult<()> {
        db.execute("INSERT INTO receipts(actor,session,kind,request_key,digest,body) VALUES(?1,?2,?3,?4,?5,?6)",params![self.actor,self.session,self.kind,self.key,self.digest,encoded(value)?])?;
        Ok(())
    }
}
fn policy(policy: &TaskPolicy) -> WorkResult<()> {
    if policy.allowed_agents.is_empty()
        || policy.allowed_agents.len() > 32
        || policy.max_workers > 16
        || policy.allowed_agents.iter().any(|s| !bounded(s, 64))
    {
        return Err(invalid());
    }
    Ok(())
}
impl WorkStore {
    pub fn in_memory() -> WorkResult<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }
    /// The configured parent is trusted local state, made private before SQLite
    /// opens its pathname. Ancestor aliases are canonicalized intentionally (for
    /// example macOS /var). This is not an atomic SQLite adoption of our file
    /// descriptor: arbitrary same-UID ancestor replacement is outside this fence.
    pub fn open(path: &Path) -> WorkResult<Self> {
        let parent = path.parent().ok_or_else(invalid)?;
        let filename = path.file_name().ok_or_else(invalid)?;
        std::fs::create_dir_all(parent).map_err(|_| WorkError(FailureCode::StorageUnavailable))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
            let unavailable = |_| WorkError(FailureCode::StorageUnavailable);
            if std::fs::symlink_metadata(parent)
                .map_err(unavailable)?
                .file_type()
                .is_symlink()
            {
                return Err(invalid());
            }
            let parent = std::fs::canonicalize(parent).map_err(unavailable)?;
            let parent_handle = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&parent)
                .map_err(unavailable)?;
            let parent_identity = parent_handle.metadata().map_err(unavailable)?;
            // SAFETY: geteuid has no preconditions and does not alter process state.
            let owner = unsafe { libc::geteuid() };
            if !parent_identity.is_dir() || parent_identity.uid() != owner {
                return Err(invalid());
            }
            parent_handle
                .set_permissions(std::fs::Permissions::from_mode(0o700))
                .map_err(unavailable)?;
            let path = parent.join(filename);
            // Refuse existing special files without opening them at all. The
            // descriptor checks below repeat this after the nonblocking open.
            match std::fs::symlink_metadata(&path) {
                Ok(existing)
                    if !existing.is_file() || existing.nlink() != 1 || existing.uid() != owner =>
                {
                    return Err(invalid())
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(unavailable(error)),
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .map_err(unavailable)?;
            let identity = file.metadata().map_err(unavailable)?;
            // Check before chmod: a FIFO must not block, and a hard link must not
            // let state initialization alter another file's permissions or bytes.
            if !identity.is_file() || identity.nlink() != 1 || identity.uid() != owner {
                return Err(invalid());
            }
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(unavailable)?;
            // Preflight created the file. SQLite must neither recreate it after a
            // disappearance nor interpret its name as a URI or follow a symlink.
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
            let connection = Connection::open_with_flags(&path, flags)?;
            let current_parent = std::fs::symlink_metadata(&parent).map_err(unavailable)?;
            let current = std::fs::symlink_metadata(&path).map_err(unavailable)?;
            let held = file.metadata().map_err(unavailable)?;
            if !current_parent.is_dir()
                || current_parent.dev() != parent_identity.dev()
                || current_parent.ino() != parent_identity.ino()
                || current_parent.uid() != owner
                || current_parent.mode() & 0o777 != 0o700
                || !current.is_file()
                || current.dev() != identity.dev()
                || current.ino() != identity.ino()
                || current.nlink() != 1
                || current.uid() != owner
                || current.mode() & 0o777 != 0o600
                || held.nlink() != 1
            {
                return Err(invalid());
            }
            // Keep both preflight handles alive through initialization. The private
            // parent is the protection against other users changing SQLite sidecars.
            let store = Self::initialize(connection)?;
            drop(file);
            drop(parent_handle);
            Ok(store)
        }
        #[cfg(not(unix))]
        {
            let _ = filename;
            Self::initialize(Connection::open(path)?)
        }
    }
    fn initialize(connection: Connection) -> WorkResult<Self> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 2 {
            return Err(WorkError(FailureCode::StorageUnavailable));
        }
        connection.execute_batch("BEGIN IMMEDIATE;
          CREATE TABLE IF NOT EXISTS records(id TEXT PRIMARY KEY, session TEXT NOT NULL, kind TEXT NOT NULL, task_id TEXT NOT NULL REFERENCES records(id) DEFERRABLE INITIALLY DEFERRED, body TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS records_scope ON records(session,kind,task_id);
          CREATE TABLE IF NOT EXISTS receipts(actor TEXT NOT NULL,session TEXT NOT NULL,kind TEXT NOT NULL,request_key TEXT NOT NULL,digest TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(actor,session,kind,request_key));
          CREATE TABLE IF NOT EXISTS changes(cursor INTEGER PRIMARY KEY AUTOINCREMENT,session TEXT NOT NULL,task_id TEXT NOT NULL REFERENCES records(id),revision INTEGER NOT NULL,kind TEXT NOT NULL,entity_id TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS changes_scope ON changes(session,cursor);
          CREATE TABLE IF NOT EXISTS work_inputs(id TEXT PRIMARY KEY,actor TEXT NOT NULL,session TEXT NOT NULL,repo_path TEXT NOT NULL,claimed_task TEXT REFERENCES records(id) DEFERRABLE INITIALLY DEFERRED,body TEXT NOT NULL);
          CREATE INDEX IF NOT EXISTS work_inputs_scope ON work_inputs(actor,session,repo_path);
          CREATE TABLE IF NOT EXISTS work_delivery_inputs(operation_id TEXT PRIMARY KEY REFERENCES records(id),request_digest TEXT NOT NULL,final_prompt_digest TEXT NOT NULL);
          PRAGMA user_version=2; COMMIT;")?;
        let has_version: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('work_delivery_inputs') WHERE name='instruction_version')", [], |row| row.get(0))?;
        if !has_version {
            connection.execute(
                "ALTER TABLE work_delivery_inputs ADD COLUMN instruction_version TEXT",
                [],
            )?;
        }
        let mut store = Self {
            connection,
            summaries_ready: false,
        };
        // An optional projection failure must not disable the legacy record API.
        let _ = store.rebuild_summaries();
        store.recover()?;
        Ok(store)
    }
    fn recover(&mut self) -> WorkResult<()> {
        let tx = self.connection.transaction()?;
        let pending: Vec<(String, String)> = {
            let mut s = tx.prepare("SELECT session,body FROM records WHERE kind='operation'")?;
            let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        for (session, json) in pending {
            let mut op: Operation = serde_json::from_str(&json)?;
            if op.state == OperationState::Submitting {
                op.state = OperationState::Unconfirmed;
                op.failure_code = Some(FailureCode::DeliveryUnconfirmed);
                put(&tx, &session, "operation", &op.id, &op.task_id, &op)?;
                let mut task: Task = load(&tx, &session, "task", &op.task_id)?;
                let now = task.updated_at_ms;
                changed(&tx, &mut task, "operation_recovered", &op.id, now)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn create_task(
        &mut self,
        actor: &str,
        session: &str,
        key: &str,
        input: CreateTask,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        self.create_task_with_inputs(actor, session, key, input, Vec::new(), now)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn create_task_with_inputs(
        &mut self,
        actor: &str,
        session: &str,
        key: &str,
        input: CreateTask,
        input_refs: Vec<InputRef>,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        self.create_task_inner(actor, session, key, input, input_refs, None, now)
    }
    #[allow(clippy::too_many_arguments)]
    fn create_task_inner(
        &mut self,
        actor: &str,
        session: &str,
        key: &str,
        input: CreateTask,
        input_refs: Vec<InputRef>,
        delegated: Option<delegation::DelegatedCreate>,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        inputs::validate_refs(&input_refs)?;
        policy(&input.policy)?;
        if !Path::new(&input.repo_path).is_absolute()
            || !bounded(&input.repo_path, 4096)
            || !bounded(&input.title, 240)
            || !bounded(&input.brief, 65536)
        {
            return Err(invalid());
        }
        let request = Request {
            actor,
            session,
            key,
            kind: "create_task",
            digest: if let Some(context) = &delegated {
                payload_digest(&(&input, &input_refs, context))?
            } else {
                inputs::create_request_digest(&input, &input_refs)?
            },
        };
        let tx = self.connection.transaction()?;
        if let Some(value) = request.replay(&tx)? {
            return Ok(Mutation {
                value,
                replayed: true,
            });
        }
        if let Some(context) = &delegated {
            delegation::authorize_create(&tx, session, &input, context)?;
        }
        let count: u64 =
            tx.query_row("SELECT COUNT(*) FROM records WHERE kind='task'", [], |r| {
                r.get(0)
            })?;
        if count >= 10000 {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        if let Some(parent_id) = &input.parent_task_id {
            let parent: Task = load(&tx, session, "task", parent_id)?;
            if parent.repo_path != input.repo_path
                || input.policy.max_workers > parent.policy.max_workers
                || input
                    .policy
                    .allowed_agents
                    .iter()
                    .any(|a| !parent.policy.allowed_agents.contains(a))
            {
                return Err(WorkError(FailureCode::ScopeMismatch));
            }
            let mut next = parent.parent_task_id;
            let mut depth = 1;
            while let Some(p) = next {
                depth += 1;
                if depth >= 16 {
                    return Err(WorkError(FailureCode::ResourceLimit));
                }
                next = load::<Task>(&tx, session, "task", &p)?.parent_task_id;
            }
        }
        let mut task = Task {
            delegation: DelegationState::default(),
            dependencies: delegated
                .as_ref()
                .map(|value| value.dependencies.clone())
                .unwrap_or_default(),
            input_refs: Vec::new(),
            id: id(),
            session_id: session.into(),
            repo_path: input.repo_path,
            title: input.title,
            brief: input.brief,
            parent_task_id: input.parent_task_id,
            revision: 0,
            created_at_ms: now,
            updated_at_ms: now,
            paused: false,
            policy: input.policy,
        };
        let tid = task.id.clone();
        if let Some(context) = &delegated {
            let parent: Task = load(&tx, session, "task", &context.fence.coordinator_task_id)?;
            task.delegation.policy = DelegationPolicy {
                enabled: false,
                max_children: parent.delegation.policy.max_children,
                max_depth: parent.delegation.policy.max_depth.saturating_sub(1),
                dependency_requirement: parent.delegation.policy.dependency_requirement,
            };
        }
        delegation::validate_dependencies(&tx, session, &task, &task.dependencies)?;
        if let Some(context) = &delegated {
            let parent: Task = load(&tx, session, "task", &context.fence.coordinator_task_id)?;
            task.input_refs =
                inputs::resolve_parent_shares(&tx, actor, session, &parent, &input_refs, now)?;
            inputs::insert_shares(
                &tx,
                session,
                &parent,
                &tid,
                &context.fence,
                &task.input_refs,
                now,
            )?;
        } else {
            task.input_refs =
                inputs::resolve(&tx, actor, session, &task.repo_path, None, &input_refs, now)?;
            inputs::claim(&tx, session, &tid, &task.input_refs)?;
        }
        changed(&tx, &mut task, "task_created", &tid, now)?;
        if let Some(context) = &delegated {
            put(
                &tx,
                session,
                "delegated_origin",
                &id(),
                &tid,
                &delegation::DelegatedOrigin {
                    actor_id: actor.into(),
                    fence: context.fence.clone(),
                },
            )?;
            let mut parent: Task = load(&tx, session, "task", &context.fence.coordinator_task_id)?;
            changed(&tx, &mut parent, "child_created", &tid, now)?;
        }
        request.save(&tx, &task)?;
        tx.commit()?;
        Ok(Mutation {
            value: task,
            replayed: false,
        })
    }
    pub fn list_tasks(
        &self,
        session: &str,
        after_id: Option<&str>,
        limit: u32,
    ) -> WorkResult<Vec<Task>> {
        if limit == 0 || limit > 100 {
            return Err(invalid());
        }
        let mut s=self.connection.prepare("SELECT body FROM records WHERE session=?1 AND kind='task' AND id>?2 ORDER BY id LIMIT ?3")?;
        let rows = s.query_map(params![session, after_id.unwrap_or(""), limit], |r| {
            r.get::<_, String>(0)
        })?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn detail(&mut self, session: &str, task_id: &str) -> WorkResult<TaskDetail> {
        let tx = self.connection.transaction()?;
        let value = TaskDetail {
            task: load(&tx, session, "task", task_id)?,
            attempts: records(&tx, session, "attempt", task_id)?,
            operations: records(&tx, session, "operation", task_id)?,
            results: records(&tx, session, "result", task_id)?,
            reviews: records(&tx, session, "review", task_id)?,
            cursor: cursor(&tx)?,
        };
        tx.commit()?;
        Ok(value)
    }
    /// Absence means no committed receipt is visible in this snapshot. It does not prove
    /// an in-flight request had no effects, and callers must never infer retry authority.
    pub fn get_receipt(
        &mut self,
        actor: &str,
        session: &str,
        kind: OperationKind,
        key: &str,
    ) -> WorkResult<RequestReceipt> {
        if !bounded(actor, 256) || !bounded(session, 256) || !bounded(key, 128) {
            return Err(invalid());
        }
        let serialized_kind = serde_json::to_value(&kind)?;
        let kind_key = serialized_kind.as_str().ok_or_else(invalid)?;
        let tx = self.connection.transaction()?;
        let body:Option<String>=tx.query_row("SELECT body FROM receipts WHERE actor=?1 AND session=?2 AND kind=?3 AND request_key=?4",params![actor,session,kind_key,key],|row|row.get(0)).optional()?;
        let body = body.ok_or(WorkError(FailureCode::NotFound))?;
        let value = match kind {
            OperationKind::StartAttempt
            | OperationKind::DeliverPrompt
            | OperationKind::InterruptAttempt => {
                let original: Operation = serde_json::from_str(&body)?;
                serde_json::to_value(load::<Operation>(&tx, session, "operation", &original.id)?)?
            }
            OperationKind::ReconcileAttempt => {
                let original: Operation = serde_json::from_str(&body)?;
                match reconciliation(&tx, session, &original.id)? {
                    Some(receipt) => {
                        serde_json::json!({"receipt_type":"reconciliation", "receipt":receipt})
                    }
                    None => {
                        serde_json::json!({"receipt_type":"operation", "operation":load::<Operation>(&tx, session, "operation", &original.id)?})
                    }
                }
            }
            _ => serde_json::from_str(&body)?,
        };
        tx.commit()?;
        Ok(RequestReceipt { kind, value })
    }
    pub fn get_operation(&self, session: &str, operation_id: &str) -> WorkResult<Operation> {
        load(&self.connection, session, "operation", operation_id)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_attempt(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: NewAttempt,
        digest: &str,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        self.prepare_attempt_inner(
            actor,
            session,
            task_id,
            key,
            expected_revision,
            input,
            digest,
            None,
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn prepare_attempt_inner(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: NewAttempt,
        digest: &str,
        fence: Option<DelegationFence>,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        if !bounded(&input.agent_kind, 64) || !bounded(digest, 128) {
            return Err(invalid());
        }
        let request = Request {
            actor,
            session,
            key,
            kind: "start_attempt",
            digest: if fence.is_some() {
                payload_digest(&(task_id, expected_revision, &input, digest, &fence))?
            } else {
                payload_digest(&(task_id, expected_revision, &input, digest))?
            },
        };
        let tx = self.connection.transaction()?;
        if let Some(old) = request.replay::<Operation>(&tx)? {
            return Ok(Mutation {
                value: load(&tx, session, "operation", &old.id)?,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, expected_revision)?;
        if let Some(fence) = &fence {
            delegation::authorize_child(&tx, session, fence, &task)?;
        }
        launch_allowed(&tx, &task)?;
        let dependency_snapshot = delegation::dependency_snapshot(&tx, session, &task)?;
        if !task.policy.allowed_agents.contains(&input.agent_kind) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        delegation_budget(&tx, &task, &input.role)?;
        let attempts: Vec<Attempt> = records(&tx, session, "attempt", task_id)?;
        let attempts: Vec<&Attempt> = attempts
            .iter()
            .filter(|attempt| reservation_active(attempt))
            .collect();
        if input.role == AttemptRole::Worker
            && !attempts
                .iter()
                .any(|a| a.role == AttemptRole::Lead && a.binding.instance_id.is_some())
        {
            return Err(WorkError(FailureCode::NotReady));
        }
        // Reservations survive unknown native outcomes. Explicit lifecycle reconciliation is required to release them.
        if input.role == AttemptRole::Worker
            && attempts
                .iter()
                .filter(|a| a.role == AttemptRole::Worker)
                .count()
                >= task.policy.max_workers as usize
        {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        if input.role == AttemptRole::Lead && attempts.iter().any(|a| a.role == AttemptRole::Lead) {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        let attempt = Attempt {
            lifecycle: AttemptLifecycle {
                launch_phase: LaunchPhase::NotDispatched,
                ..Default::default()
            },
            id: id(),
            task_id: task_id.into(),
            agent_kind: input.agent_kind,
            role: input.role,
            binding: NativeBinding::default(),
            created_at_ms: now,
        };
        put(&tx, session, "attempt", &attempt.id, task_id, &attempt)?;
        let mut operation =
            new_operation(task_id, Some(attempt.id), OperationKind::StartAttempt, now);
        operation.delegation_fence = fence;
        operation.dependency_snapshot = dependency_snapshot;
        put(
            &tx,
            session,
            "operation",
            &operation.id,
            task_id,
            &operation,
        )?;
        reserve_fact(&tx, session, &operation, "reserved_start_refusal")?;
        changed(&tx, &mut task, "attempt_prepared", &operation.id, now)?;
        request.save(&tx, &operation)?;
        tx.commit()?;
        Ok(Mutation {
            value: operation,
            replayed: false,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_delivery(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        key: &str,
        expected_revision: u64,
        expected_instance: &str,
        text: &str,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        self.prepare_delivery_with_instructions(
            actor,
            session,
            task_id,
            attempt_id,
            key,
            expected_revision,
            expected_instance,
            text,
            None,
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_delivery_with_instructions(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        key: &str,
        expected_revision: u64,
        expected_instance: &str,
        text: &str,
        instruction_version: Option<&str>,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        self.prepare_delivery_input_inner(
            actor,
            session,
            task_id,
            attempt_id,
            key,
            expected_revision,
            expected_instance,
            text,
            instruction_version,
            Vec::new(),
            None,
            None,
            None,
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_delivery_with_inputs(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        key: &str,
        expected_revision: u64,
        expected_instance: &str,
        text: &str,
        instruction_version: Option<&str>,
        input_refs: Vec<InputRef>,
        expected_inputs: &[FrozenInputRef],
        final_prompt_digest: &str,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        self.prepare_delivery_input_inner(
            actor,
            session,
            task_id,
            attempt_id,
            key,
            expected_revision,
            expected_instance,
            text,
            instruction_version,
            input_refs,
            Some(expected_inputs),
            Some(final_prompt_digest),
            None,
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn prepare_delivery_input_inner(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        key: &str,
        expected_revision: u64,
        expected_instance: &str,
        text: &str,
        instruction_version: Option<&str>,
        input_refs: Vec<InputRef>,
        expected_inputs: Option<&[FrozenInputRef]>,
        final_prompt_digest: Option<&str>,
        fence: Option<DelegationFence>,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        inputs::validate_refs(&input_refs)?;
        if final_prompt_digest.is_some_and(|digest| !inputs::valid_digest(digest)) {
            return Err(invalid());
        }
        let mut client_digest = inputs::delivery_request_digest(
            task_id,
            attempt_id,
            expected_revision,
            expected_instance,
            text,
            instruction_version,
            &input_refs,
        )?;
        if let Some(fence) = &fence {
            client_digest = payload_digest(&(&client_digest, fence))?;
        }
        if expected_inputs.is_some() {
            let replay = if let Some(fence) = &fence {
                self.replay_delegated_delivery(
                    actor,
                    session,
                    task_id,
                    attempt_id,
                    key,
                    expected_revision,
                    expected_instance,
                    text,
                    instruction_version,
                    &input_refs,
                    fence,
                )?
            } else {
                self.replay_delivery_with_inputs(
                    actor,
                    session,
                    task_id,
                    attempt_id,
                    key,
                    expected_revision,
                    expected_instance,
                    text,
                    instruction_version,
                    &input_refs,
                )?
            };
            if let Some(replay) = replay {
                return Ok(replay);
            }
        }
        if !bounded(text, 65536) || !bounded(expected_instance, 512) {
            return Err(invalid());
        }
        if instruction_version.is_some_and(|v| !bounded(v, 64)) {
            return Err(invalid());
        }
        let request = Request {
            actor,
            session,
            key,
            kind: "deliver_prompt",
            digest: if expected_inputs.is_some() {
                payload_digest(&(&client_digest, expected_inputs, final_prompt_digest))?
            } else if fence.is_some() {
                payload_digest(&(
                    task_id,
                    attempt_id,
                    expected_revision,
                    expected_instance,
                    text,
                    instruction_version,
                    &fence,
                ))?
            } else {
                inputs::legacy_delivery_digest(
                    task_id,
                    attempt_id,
                    expected_revision,
                    expected_instance,
                    text,
                    instruction_version,
                )?
            },
        };
        let tx = self.connection.transaction()?;
        if expected_inputs.is_some() {
            let legacy = if input_refs.is_empty() {
                Some(inputs::legacy_delivery_digest(
                    task_id,
                    attempt_id,
                    expected_revision,
                    expected_instance,
                    text,
                    instruction_version,
                )?)
            } else {
                None
            };
            let replay = if let Some(fence) = &fence {
                delegation::replay_delivery(&tx, actor, session, key, &client_digest, fence)?
            } else {
                inputs::replay_delivery_request(
                    &tx,
                    actor,
                    session,
                    key,
                    &client_digest,
                    legacy.as_deref(),
                )?
            };
            if let Some(old) = replay {
                return Ok(old);
            }
        }
        if let Some(old) = request.replay::<Operation>(&tx)? {
            return Ok(Mutation {
                value: load(&tx, session, "operation", &old.id)?,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, expected_revision)?;
        let attempt: Attempt = load(&tx, session, "attempt", attempt_id)?;
        if let Some(fence) = &fence {
            delegation::authorize_child(&tx, session, fence, &task)?;
        }
        if attempt.task_id != task_id {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        if !reservation_active(&attempt)
            || attempt.binding.instance_id.as_deref() != Some(expected_instance)
        {
            return Err(WorkError(FailureCode::InstanceChanged));
        }
        let operations: Vec<Operation> = records(&tx, session, "operation", task_id)?;
        if operations.iter().any(|op| {
            op.attempt_id.as_deref() == Some(attempt_id)
                && matches!(
                    op.kind,
                    OperationKind::StartAttempt
                        | OperationKind::DeliverPrompt
                        | OperationKind::InterruptAttempt
                )
                && matches!(
                    op.state,
                    OperationState::Prepared
                        | OperationState::Submitting
                        | OperationState::Unconfirmed
                )
        }) {
            return Err(WorkError(FailureCode::NotReady));
        }
        let mut operation = new_operation(
            task_id,
            Some(attempt_id.into()),
            OperationKind::DeliverPrompt,
            now,
        );
        operation.delegation_fence = fence;
        operation.input_refs = if let Some(fence) = &operation.delegation_fence {
            delegation::resolve_child_inputs(&tx, actor, session, fence, &task, &input_refs, now)?
        } else {
            inputs::resolve(
                &tx,
                actor,
                session,
                &task.repo_path,
                Some(task_id),
                &input_refs,
                now,
            )?
        };
        if expected_inputs.is_some_and(|expected| expected != operation.input_refs) {
            return Err(WorkError(FailureCode::ArtifactChanged));
        }
        inputs::claim(&tx, session, task_id, &operation.input_refs)?;
        if !operations.iter().any(|op| {
            op.attempt_id.as_deref() == Some(attempt_id)
                && op.kind == OperationKind::DeliverPrompt
                && op.state != OperationState::Refused
        }) {
            operation.bootstrap_version = instruction_version.map(str::to_owned);
        }
        operation.resources = attempt.binding;
        put(
            &tx,
            session,
            "operation",
            &operation.id,
            task_id,
            &operation,
        )?;
        changed(&tx, &mut task, "delivery_prepared", &operation.id, now)?;
        if let Some(final_digest) = final_prompt_digest {
            tx.execute("INSERT INTO work_delivery_inputs(operation_id,request_digest,final_prompt_digest,instruction_version) VALUES(?1,?2,?3,?4)",params![operation.id,client_digest,final_digest,encoded(&instruction_version)?])?;
        }
        request.save(&tx, &operation)?;
        tx.commit()?;
        Ok(Mutation {
            value: operation,
            replayed: false,
        })
    }
    pub fn assert_launch_allowed(&self, session: &str, task_id: &str) -> WorkResult<()> {
        let task: Task = load(&self.connection, session, "task", task_id)?;
        launch_allowed(&self.connection, &task)
    }
    /// Sole durable permission to dispatch the native assistant-start call.
    pub fn claim_start_dispatch(
        &mut self,
        session: &str,
        operation_id: &str,
        now: i64,
    ) -> WorkResult<Operation> {
        self.claim_start_dispatch_inner(session, operation_id, None, now)
    }
    fn claim_start_dispatch_inner(
        &mut self,
        session: &str,
        operation_id: &str,
        target: Option<delegation::OperationTarget>,
        now: i64,
    ) -> WorkResult<Operation> {
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if let Some(target) = &target {
            delegation::validate_operation_target(&tx, session, &op, target)?;
        }
        let mut attempt = start_attempt(&tx, session, &op)?;
        if op.state != OperationState::Submitting
            || !reservation_active(&attempt)
            || attempt.lifecycle.launch_phase != LaunchPhase::NotDispatched
        {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        let task: Task = load(&tx, session, "task", &op.task_id)?;
        if let Some(fence) = &op.delegation_fence {
            delegation::authorize_child(&tx, session, fence, &task)?;
        }
        launch_allowed(&tx, &task)?;
        op.dependency_snapshot = delegation::dependency_snapshot(&tx, session, &task)?;
        reserve_fact(&tx, session, &op, "reserved_start_refusal")?;
        attempt.lifecycle.launch_phase = LaunchPhase::DispatchClaimed;
        put(&tx, session, "attempt", &attempt.id, &op.task_id, &attempt)?;
        op.updated_at_ms = now;
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
    pub fn confirm_start_launch(
        &mut self,
        session: &str,
        operation_id: &str,
        resources: NativeBinding,
        owner_epoch: &str,
        now: i64,
    ) -> WorkResult<Operation> {
        if !native_identity(owner_epoch)
            || !resources
                .instance_id
                .as_deref()
                .is_some_and(native_identity)
        {
            return Err(invalid());
        }
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        let mut attempt = start_attempt(&tx, session, &op)?;
        if op.state != OperationState::Submitting
            || !reservation_active(&attempt)
            || attempt.lifecycle.launch_phase != LaunchPhase::DispatchClaimed
            || start_refusal(&tx, session, operation_id)?.is_some()
        {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        merge_binding(&mut op.resources, resources)?;
        merge_binding(&mut attempt.binding, op.resources.clone())?;
        attempt.lifecycle.launch_phase = LaunchPhase::LaunchConfirmed;
        attempt.lifecycle.native_owner_epoch = Some(owner_epoch.into());
        put(&tx, session, "attempt", &attempt.id, &op.task_id, &attempt)?;
        op.updated_at_ms = now;
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
    pub fn record_start_refusal(
        &mut self,
        session: &str,
        operation_id: &str,
        owner_epoch: &str,
        receipt_id: &str,
        now: i64,
    ) -> WorkResult<()> {
        if !native_identity(owner_epoch) || !native_identity(receipt_id) {
            return Err(invalid());
        }
        let tx = self.connection.transaction()?;
        let op: Operation = load(&tx, session, "operation", operation_id)?;
        let attempt = start_attempt(&tx, session, &op)?;
        let proof = NativeStartRefusal {
            start_operation_id: operation_id.into(),
            owner_epoch: owner_epoch.into(),
            receipt_id: receipt_id.into(),
        };
        if let Some(old) = start_refusal(&tx, session, operation_id)? {
            return if old == proof {
                Ok(())
            } else {
                Err(WorkError(FailureCode::InstanceChanged))
            };
        }
        if op.state != OperationState::Submitting
            || attempt.lifecycle.launch_phase != LaunchPhase::DispatchClaimed
            || attempt.binding.instance_id.is_some()
            || !reservation_active(&attempt)
        {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        fulfill_fact(
            &tx,
            session,
            &op,
            "reserved_start_refusal",
            "start_refusal",
            &proof,
        )?;
        let mut task: Task = load(&tx, session, "task", &op.task_id)?;
        changed(&tx, &mut task, "native_start_refused", &op.id, now)?;
        tx.commit()?;
        Ok(())
    }
    pub fn get_start_refusal(
        &self,
        session: &str,
        operation_id: &str,
    ) -> WorkResult<Option<NativeStartRefusal>> {
        let _: Operation = load(&self.connection, session, "operation", operation_id)?;
        start_refusal(&self.connection, session, operation_id)
    }
    pub fn get_reconciliation(
        &self,
        session: &str,
        operation_id: &str,
    ) -> WorkResult<ReconciliationReceipt> {
        reconciliation(&self.connection, session, operation_id)?
            .ok_or(WorkError(FailureCode::NotFound))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_reconciliation(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        input: ReconcileInput,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        for value in [
            &input.expected_instance_id,
            &input.expected_native_owner_epoch,
        ]
        .into_iter()
        .flatten()
        {
            if !native_identity(value) {
                return Err(invalid());
            }
        }
        let request = Request {
            actor,
            session,
            key: &input.request_key,
            kind: "reconcile_attempt",
            digest: payload_digest(&(task_id, attempt_id, &input))?,
        };
        let tx = self.connection.transaction()?;
        if let Some(old) = request.replay::<Operation>(&tx)? {
            return Ok(Mutation {
                value: load(&tx, session, "operation", &old.id)?,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, input.expected_revision)?;
        let attempt: Attempt = load(&tx, session, "attempt", attempt_id)?;
        reconcile_binding(&attempt, task_id, &input)?;
        let op = new_operation(
            task_id,
            Some(attempt_id.into()),
            OperationKind::ReconcileAttempt,
            now,
        );
        put(&tx, session, "operation", &op.id, task_id, &op)?;
        put(
            &tx,
            session,
            "reconciliation_intent",
            &id(),
            task_id,
            &ReconciliationIntent {
                operation_id: op.id.clone(),
                input: input.clone(),
            },
        )?;
        reserve_fact(&tx, session, &op, "reserved_reconciliation")?;
        changed(&tx, &mut task, "reconciliation_prepared", &op.id, now)?;
        request.save(&tx, &op)?;
        tx.commit()?;
        Ok(Mutation {
            value: op,
            replayed: false,
        })
    }
    /// Caller serializes local authorization with this transaction using the grant guard.
    pub fn finish_reconciliation(
        &mut self,
        session: &str,
        operation_id: &str,
        expected_revision: u64,
        evidence: ReconciliationEvidence,
        now: i64,
    ) -> WorkResult<ReconciliationReceipt> {
        let tx = self.connection.transaction()?;
        if let Some(old) = reconciliation(&tx, session, operation_id)? {
            return Ok(old);
        }
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if op.kind != OperationKind::ReconcileAttempt || op.state != OperationState::Submitting {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        let mut task: Task = load(&tx, session, "task", &op.task_id)?;
        revision(&task, expected_revision)?;
        let mut attempt: Attempt = load(
            &tx,
            session,
            "attempt",
            op.attempt_id.as_deref().ok_or_else(invalid)?,
        )?;
        let intent: ReconciliationIntent =
            record_by_operation(&tx, session, "reconciliation_intent", operation_id)?
                .ok_or_else(invalid)?;
        reconcile_binding(&attempt, &op.task_id, &intent.input)?;
        let mut starts: Vec<Operation> = records(&tx, session, "operation", &op.task_id)?;
        let start = starts
            .iter_mut()
            .find(|candidate| {
                candidate.kind == OperationKind::StartAttempt
                    && candidate.attempt_id.as_deref() == Some(&attempt.id)
            })
            .ok_or_else(invalid)?;
        let (observation, release) = if !reservation_active(&attempt) {
            (LifecycleObservation::AlreadyReleased, None)
        } else {
            match evidence {
                ReconciliationEvidence::Unknown => (LifecycleObservation::Unknown, None),
                ReconciliationEvidence::NotDispatched => {
                    if attempt.lifecycle.launch_phase != LaunchPhase::NotDispatched
                        || attempt.binding.instance_id.is_some()
                        || start.state == OperationState::Acknowledged
                    {
                        return Err(WorkError(FailureCode::NotReady));
                    }
                    start.state = OperationState::Refused;
                    start.failure_code = Some(FailureCode::NotReady);
                    start.updated_at_ms = now;
                    put(&tx, session, "operation", &start.id, &op.task_id, start)?;
                    (
                        LifecycleObservation::NotStarted,
                        Some((
                            ReleaseReason::StartupNotDispatched,
                            ReleaseEvidence::GatewayDispatchFence {
                                start_operation_id: start.id.clone(),
                            },
                        )),
                    )
                }
                ReconciliationEvidence::NativeNotStarted {
                    start_operation_id,
                    owner_epoch,
                    receipt_id,
                } => {
                    let proof = NativeStartRefusal {
                        start_operation_id,
                        owner_epoch,
                        receipt_id,
                    };
                    if proof.start_operation_id != start.id
                        || attempt.lifecycle.launch_phase != LaunchPhase::DispatchClaimed
                        || attempt.binding.instance_id.is_some()
                        || start_refusal(&tx, session, &start.id)?.as_ref() != Some(&proof)
                    {
                        return Err(WorkError(FailureCode::NotReady));
                    }
                    (
                        LifecycleObservation::NotStarted,
                        Some((
                            ReleaseReason::StartupRefusedWithoutProcess,
                            ReleaseEvidence::NativeStartRefusal {
                                start_operation_id: start.id.clone(),
                                native_owner_epoch: proof.owner_epoch,
                                native_receipt_id: proof.receipt_id,
                            },
                        )),
                    )
                }
                ReconciliationEvidence::Live {
                    instance_id,
                    owner_epoch,
                } => {
                    match_native_binding(&attempt, &instance_id, &owner_epoch)?;
                    (LifecycleObservation::Live, None)
                }
                ReconciliationEvidence::Exited {
                    instance_id,
                    owner_epoch,
                    receipt_id,
                } => {
                    match_native_binding(&attempt, &instance_id, &owner_epoch)?;
                    if !native_identity(&receipt_id) {
                        return Err(invalid());
                    }
                    (
                        LifecycleObservation::Exited,
                        Some((
                            ReleaseReason::OwnedProcessExited,
                            ReleaseEvidence::NativeExitTombstone {
                                instance_id,
                                native_owner_epoch: owner_epoch,
                                native_receipt_id: receipt_id,
                            },
                        )),
                    )
                }
            }
        };
        if let Some((reason, evidence)) = release {
            attempt.lifecycle.reservation = Reservation::Released;
            attempt.lifecycle.release = Some(AttemptRelease {
                reason,
                evidence,
                reconciliation_operation_id: op.id.clone(),
                released_at_ms: now,
            });
            put(&tx, session, "attempt", &attempt.id, &op.task_id, &attempt)?;
        }
        op.state = OperationState::Acknowledged;
        op.updated_at_ms = now;
        put(&tx, session, "operation", &op.id, &op.task_id, &op)?;
        changed(&tx, &mut task, "attempt_reconciled", &op.id, now)?;
        let receipt = ReconciliationReceipt {
            operation_id: op.id.clone(),
            attempt_id: attempt.id,
            observation,
            reservation: attempt.lifecycle.reservation,
            release: attempt.lifecycle.release,
            task_revision: task.revision,
        };
        fulfill_fact(
            &tx,
            session,
            &op,
            "reserved_reconciliation",
            "reconciliation",
            &receipt,
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    /// Cancel a recorded intent only while it is proven not to have entered native execution.
    pub fn refuse_prepared_operation(
        &mut self,
        session: &str,
        operation_id: &str,
        code: FailureCode,
        now: i64,
    ) -> WorkResult<Operation> {
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if op.state != OperationState::Prepared {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        op.state = OperationState::Refused;
        op.failure_code = Some(code);
        op.updated_at_ms = now;
        if op.kind == OperationKind::InterruptAttempt {
            fulfill_fact(
                &tx,
                session,
                &op,
                "reserved_interruption",
                "interruption_fact",
                &serde_json::json!({"operation_id":op.id,"state":op.state,"failure_code":op.failure_code,"receipt":null}),
            )?;
        }
        if op.kind == OperationKind::StartAttempt && empty_binding(&op.resources) {
            let mut attempt = start_attempt(&tx, session, &op)?;
            if attempt.lifecycle.launch_phase == LaunchPhase::NotDispatched {
                attempt.lifecycle.reservation = Reservation::Released;
                attempt.lifecycle.release = Some(AttemptRelease {
                    reason: ReleaseReason::StartupNotDispatched,
                    evidence: ReleaseEvidence::GatewayDispatchFence {
                        start_operation_id: op.id.clone(),
                    },
                    reconciliation_operation_id: op.id.clone(),
                    released_at_ms: now,
                });
                put(&tx, session, "attempt", &attempt.id, &op.task_id, &attempt)?;
            }
        }
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
    pub fn begin_operation(
        &mut self,
        session: &str,
        operation_id: &str,
        now: i64,
    ) -> WorkResult<Operation> {
        self.begin_operation_inner(session, operation_id, None, now)
    }
    fn begin_operation_inner(
        &mut self,
        session: &str,
        operation_id: &str,
        target: Option<delegation::OperationTarget>,
        now: i64,
    ) -> WorkResult<Operation> {
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if let Some(target) = &target {
            delegation::validate_operation_target(&tx, session, &op, target)?;
        }
        if op.state != OperationState::Prepared {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        if op.kind == OperationKind::StartAttempt {
            let task: Task = load(&tx, session, "task", &op.task_id)?;
            launch_allowed(&tx, &task)?;
        }
        if op.kind == OperationKind::ReconcileAttempt {
            reserve_fact(&tx, session, &op, "reserved_reconciliation")?;
        }
        if let Some(fence) = &op.delegation_fence {
            delegation::authorize_child(
                &tx,
                session,
                fence,
                &load(&tx, session, "task", &op.task_id)?,
            )?;
        }
        if matches!(
            op.kind,
            OperationKind::StartAttempt
                | OperationKind::DeliverPrompt
                | OperationKind::InterruptAttempt
        ) {
            let attempt: Attempt = load(
                &tx,
                session,
                "attempt",
                op.attempt_id.as_deref().ok_or_else(invalid)?,
            )?;
            if !reservation_active(&attempt) {
                return Err(WorkError(FailureCode::NotReady));
            }
        }
        if op.kind == OperationKind::InterruptAttempt {
            interruptions::check_binding(&tx, session, &op)?;
        }
        op.state = OperationState::Submitting;
        op.updated_at_ms = now;
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
    /// Persist each confirmed native resource immediately, before starting the next external step.
    pub fn checkpoint_operation(
        &mut self,
        session: &str,
        operation_id: &str,
        resources: NativeBinding,
        now: i64,
    ) -> WorkResult<Operation> {
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if op.kind == OperationKind::InterruptAttempt {
            return Err(invalid());
        }
        let fenced_placement = if op.kind == OperationKind::StartAttempt
            && op.state == OperationState::Refused
            && resources.instance_id.is_none()
            && resources.target.is_none()
        {
            let attempt = start_attempt(&tx, session, &op)?;
            attempt.lifecycle.launch_phase == LaunchPhase::NotDispatched
                && attempt.lifecycle.reservation == Reservation::Released
        } else {
            false
        };
        if op.state != OperationState::Submitting && !fenced_placement {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        merge_binding(&mut op.resources, resources)?;
        op.updated_at_ms = now;
        if let Some(aid) = &op.attempt_id {
            let mut attempt: Attempt = load(&tx, session, "attempt", aid)?;
            if op.kind == OperationKind::StartAttempt
                && op.resources.instance_id.is_some()
                && attempt.lifecycle.launch_phase == LaunchPhase::NotDispatched
            {
                attempt.lifecycle.launch_phase = LaunchPhase::LegacyUnknown;
            }
            merge_binding(&mut attempt.binding, op.resources.clone())?;
            put(&tx, session, "attempt", aid, &op.task_id, &attempt)?;
        }
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
    pub fn finalize_operation(
        &mut self,
        session: &str,
        operation_id: &str,
        outcome: OperationOutcome,
        now: i64,
    ) -> WorkResult<Operation> {
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if op.kind == OperationKind::InterruptAttempt {
            return Err(invalid());
        }
        if op.state != OperationState::Submitting
            || !matches!(
                outcome.state,
                OperationState::Acknowledged
                    | OperationState::Refused
                    | OperationState::Unconfirmed
            )
        {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        merge_binding(&mut op.resources, outcome.resources)?;
        op.state = outcome.state;
        op.failure_code = outcome.failure_code;
        op.updated_at_ms = now;
        if let Some(aid) = &op.attempt_id {
            let mut attempt: Attempt = load(&tx, session, "attempt", aid)?;
            if op.kind == OperationKind::StartAttempt
                && op.resources.instance_id.is_some()
                && attempt.lifecycle.launch_phase == LaunchPhase::NotDispatched
            {
                attempt.lifecycle.launch_phase = LaunchPhase::LegacyUnknown;
            }
            merge_binding(&mut attempt.binding, op.resources.clone())?;
            if op.kind == OperationKind::StartAttempt
                && op.state == OperationState::Refused
                && empty_binding(&op.resources)
                && attempt.lifecycle.launch_phase == LaunchPhase::NotDispatched
            {
                attempt.lifecycle.reservation = Reservation::Released;
                attempt.lifecycle.release = Some(AttemptRelease {
                    reason: ReleaseReason::StartupNotDispatched,
                    evidence: ReleaseEvidence::GatewayDispatchFence {
                        start_operation_id: op.id.clone(),
                    },
                    reconciliation_operation_id: op.id.clone(),
                    released_at_ms: now,
                });
            }
            put(&tx, session, "attempt", aid, &op.task_id, &attempt)?;
        }
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
    /// Resolve a committed receipt before touching mutable source artifacts.
    pub fn replay_result(
        &self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: &ResultInput,
    ) -> WorkResult<Option<Mutation<ResultSubmission>>> {
        let request = result_request(actor, session, task_id, key, expected_revision, input)?;
        Ok(request.replay(&self.connection)?.map(|value| Mutation {
            value,
            replayed: true,
        }))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn submit_result(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: ResultInput,
        now: i64,
    ) -> WorkResult<Mutation<ResultSubmission>> {
        let request = result_request(actor, session, task_id, key, expected_revision, &input)?;
        let tx = self.connection.transaction()?;
        if let Some(value) = request.replay(&tx)? {
            return Ok(Mutation {
                value,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, expected_revision)?;
        let attempt: Attempt = load(&tx, session, "attempt", &input.attempt_id)?;
        if attempt.task_id != task_id {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        let result = ResultSubmission {
            id: id(),
            task_id: task_id.into(),
            result: input,
            created_at_ms: now,
        };
        put(&tx, session, "result", &result.id, task_id, &result)?;
        changed(&tx, &mut task, "result_submitted", &result.id, now)?;
        request.save(&tx, &result)?;
        tx.commit()?;
        Ok(Mutation {
            value: result,
            replayed: false,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn review_result(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: ReviewInput,
        now: i64,
    ) -> WorkResult<Mutation<Review>> {
        if input.message.as_ref().is_some_and(|m| !bounded(m, 16384)) {
            return Err(invalid());
        }
        let request = Request {
            actor,
            session,
            key,
            kind: "review_result",
            digest: payload_digest(&(task_id, expected_revision, &input))?,
        };
        let tx = self.connection.transaction()?;
        if let Some(value) = request.replay(&tx)? {
            return Ok(Mutation {
                value,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, expected_revision)?;
        let result: ResultSubmission = load(&tx, session, "result", &input.submission_id)?;
        if result.task_id != task_id {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        let review = Review {
            id: id(),
            task_id: task_id.into(),
            actor_id: actor.into(),
            review: input,
            created_at_ms: now,
        };
        put(&tx, session, "review", &review.id, task_id, &review)?;
        changed(&tx, &mut task, "result_reviewed", &review.id, now)?;
        request.save(&tx, &review)?;
        tx.commit()?;
        Ok(Mutation {
            value: review,
            replayed: false,
        })
    }
    /// Changes only delegation policy. This operation never sends native input.
    #[allow(clippy::too_many_arguments)]
    pub fn set_paused(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: PauseInput,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        let request = Request {
            actor,
            session,
            key,
            kind: "pause_task",
            digest: payload_digest(&(task_id, expected_revision, &input))?,
        };
        let tx = self.connection.transaction()?;
        if let Some(value) = request.replay(&tx)? {
            return Ok(Mutation {
                value,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, expected_revision)?;
        task.paused = input.paused;
        let mut operation = new_operation(task_id, None, OperationKind::PauseTask, now);
        operation.state = OperationState::Acknowledged;
        put(
            &tx,
            session,
            "operation",
            &operation.id,
            task_id,
            &operation,
        )?;
        changed(
            &tx,
            &mut task,
            "delegation_policy_changed",
            &operation.id,
            now,
        )?;
        request.save(&tx, &task)?;
        tx.commit()?;
        Ok(Mutation {
            value: task,
            replayed: false,
        })
    }

    pub fn changes(
        &mut self,
        session: &str,
        after_cursor: u64,
        limit: u32,
    ) -> WorkResult<ChangePage> {
        if limit == 0 || limit > 100 {
            return Err(invalid());
        }
        let tx = self.connection.transaction()?;
        let latest = cursor(&tx)?;
        let oldest: u64 = tx.query_row("SELECT COALESCE(MIN(cursor),0) FROM changes", [], |r| {
            r.get(0)
        })?;
        let reset_required =
            after_cursor > latest || (after_cursor > 0 && after_cursor < oldest.saturating_sub(1));
        if reset_required {
            tx.commit()?;
            return Ok(ChangePage {
                changes: Vec::new(),
                cursor: latest,
                reset_required: true,
            });
        }
        let changes = {
            let mut s=tx.prepare("SELECT cursor,task_id,revision,kind,entity_id FROM changes WHERE session=?1 AND cursor>?2 ORDER BY cursor LIMIT ?3")?;
            let rows = s.query_map(params![session, after_cursor, limit], |r| {
                Ok(TaskChange {
                    cursor: r.get(0)?,
                    task_id: r.get(1)?,
                    revision: r.get(2)?,
                    kind: r.get(3)?,
                    entity_id: r.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let next = changes.last().map_or(latest, |c| c.cursor);
        tx.commit()?;
        Ok(ChangePage {
            changes,
            cursor: next,
            reset_required,
        })
    }
}
fn new_operation(
    task: &str,
    attempt_id: Option<String>,
    kind: OperationKind,
    now: i64,
) -> Operation {
    Operation {
        interruption_receipt: None,
        interruption_owner_epoch: None,
        delegation_fence: None,
        dependency_snapshot: Vec::new(),
        input_refs: Vec::new(),
        bootstrap_version: None,
        id: id(),
        task_id: task.into(),
        attempt_id,
        kind,
        state: OperationState::Prepared,
        resources: NativeBinding::default(),
        failure_code: None,
        created_at_ms: now,
        updated_at_ms: now,
    }
}
fn save_operation(tx: &Transaction<'_>, session: &str, op: &Operation, now: i64) -> WorkResult<()> {
    put(tx, session, "operation", &op.id, &op.task_id, op)?;
    let mut task: Task = load(tx, session, "task", &op.task_id)?;
    changed(tx, &mut task, "operation_changed", &op.id, now)
}
fn merge_binding(old: &mut NativeBinding, new: NativeBinding) -> WorkResult<()> {
    for (old, new) in [
        (&mut old.workspace_id, new.workspace_id),
        (&mut old.tab_id, new.tab_id),
        (&mut old.instance_id, new.instance_id),
        (&mut old.target, new.target),
        (&mut old.pane_id, new.pane_id),
        (&mut old.worktree_path, new.worktree_path),
    ] {
        if let Some(value) = new {
            if !bounded(&value, 4096) {
                return Err(invalid());
            }
            if old.as_ref().is_some_and(|v| v != &value) {
                return Err(WorkError(FailureCode::InstanceChanged));
            }
            *old = Some(value);
        }
    }
    Ok(())
}

fn empty_binding(binding: &NativeBinding) -> bool {
    binding.workspace_id.is_none()
        && binding.tab_id.is_none()
        && binding.instance_id.is_none()
        && binding.target.is_none()
        && binding.pane_id.is_none()
        && binding.worktree_path.is_none()
}

fn result_request<'a>(
    actor: &'a str,
    session: &'a str,
    task_id: &str,
    key: &'a str,
    expected_revision: u64,
    input: &ResultInput,
) -> WorkResult<Request<'a>> {
    if !bounded(&input.summary, 16384)
        || input.artifacts.len() > 32
        || input.evidence.len() > 32
        || input.evidence.iter().any(|v| !bounded(v, 4096))
    {
        return Err(invalid());
    }
    for artifact in &input.artifacts {
        if !bounded(&artifact.path, 4096)
            || artifact.sha256.len() != 64
            || !artifact.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || artifact.size_bytes > 50 * 1024 * 1024
        {
            return Err(invalid());
        }
    }
    Ok(Request {
        actor,
        session,
        key,
        kind: "submit_result",
        digest: payload_digest(&(task_id, expected_revision, input))?,
    })
}
