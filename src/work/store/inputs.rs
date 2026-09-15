use super::*;

const INPUT_TTL_MS: i64 = 48 * 60 * 60 * 1000;
const MAX_INPUTS: u64 = 100_000;
pub(super) fn use_allowed(source: &InputUse, requested: &InputUse) -> bool {
    *source == InputUse::MayInclude || *requested == InputUse::ReferenceOnly
}
fn same_metadata(reference: &FrozenInputRef, receipt: &InputReceipt) -> bool {
    reference.input_id == receipt.input_id
        && reference.sha256 == receipt.sha256
        && reference.size_bytes == receipt.size_bytes
        && reference.name == receipt.name
        && reference.mime == receipt.mime
}
fn validate_share(
    db: &Connection,
    session: &str,
    task_id: &str,
    receipt: &InputReceipt,
    requested_use: &InputUse,
) -> WorkResult<()> {
    let shares: Vec<InputShare> = records(db, session, "input_share", task_id)?;
    let matching: Vec<_> = shares
        .iter()
        .filter(|share| share.input_id == receipt.input_id)
        .collect();
    if matching.len() != 1 {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    let share = matching[0];
    let child: Task = load(db, session, "task", task_id)?;
    let parent: Task = load(db, session, "task", &share.source_task_id)?;
    let source = parent
        .input_refs
        .iter()
        .find(|input| input.input_id == receipt.input_id)
        .ok_or(WorkError(FailureCode::ScopeMismatch))?;
    let claimed: Option<String> = db.query_row(
        "SELECT claimed_task FROM work_inputs WHERE id=?1 AND session=?2",
        params![receipt.input_id, session],
        |row| row.get(0),
    )?;
    if share.recipient_task_id != child.id
        || child.parent_task_id.as_deref() != Some(&parent.id)
        || claimed.as_deref() != Some(&parent.id)
        || child.repo_path != parent.repo_path
        || parent.repo_path != receipt.repo_path
        || !same_metadata(source, receipt)
        || share.source_use != source.use_
        || !use_allowed(&share.source_use, &share.permitted_use)
        || !use_allowed(&share.permitted_use, requested_use)
        || share.sha256 != receipt.sha256
        || share.size_bytes != receipt.size_bytes
        || share.name != receipt.name
        || share.mime != receipt.mime
    {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    Ok(())
}
pub(super) fn resolve_parent_shares(
    db: &Connection,
    actor: &str,
    session: &str,
    parent: &Task,
    refs: &[InputRef],
    now: i64,
) -> WorkResult<Vec<FrozenInputRef>> {
    for reference in refs {
        let source = parent
            .input_refs
            .iter()
            .find(|input| input.input_id == reference.input_id)
            .ok_or(WorkError(FailureCode::ScopeMismatch))?;
        if !use_allowed(&source.use_, &reference.use_) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        let claimed: Option<String> = db.query_row(
            "SELECT claimed_task FROM work_inputs WHERE id=?1 AND session=?2",
            params![reference.input_id, session],
            |row| row.get(0),
        )?;
        if claimed.as_deref() != Some(&parent.id) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
    }
    let frozen = resolve(
        db,
        actor,
        session,
        &parent.repo_path,
        Some(&parent.id),
        refs,
        now,
    )?;
    for item in &frozen {
        let source = parent
            .input_refs
            .iter()
            .find(|input| input.input_id == item.input_id)
            .ok_or(WorkError(FailureCode::ScopeMismatch))?;
        if item.sha256 != source.sha256
            || item.size_bytes != source.size_bytes
            || item.mime != source.mime
            || item.name != source.name
        {
            return Err(WorkError(FailureCode::ArtifactChanged));
        }
    }
    Ok(frozen)
}
pub(super) fn insert_shares(
    db: &Connection,
    session: &str,
    parent: &Task,
    child_id: &str,
    fence: &DelegationFence,
    refs: &[FrozenInputRef],
    now: i64,
) -> WorkResult<()> {
    if refs.len() > 9 {
        return Err(invalid());
    }
    let existing: Vec<InputShare> = records(db, session, "input_share", child_id)?;
    if existing.len() + refs.len() > 9 {
        return Err(WorkError(FailureCode::ResourceLimit));
    }
    for reference in refs {
        if existing
            .iter()
            .any(|share| share.input_id == reference.input_id)
        {
            return Err(WorkError(FailureCode::RequestKeyConflict));
        }
        let source = parent
            .input_refs
            .iter()
            .find(|input| input.input_id == reference.input_id)
            .ok_or(WorkError(FailureCode::ScopeMismatch))?;
        let share = InputShare {
            id: id(),
            source_task_id: parent.id.clone(),
            recipient_task_id: child_id.into(),
            input_id: reference.input_id.clone(),
            source_use: source.use_.clone(),
            permitted_use: reference.use_.clone(),
            sha256: reference.sha256.clone(),
            size_bytes: reference.size_bytes,
            name: reference.name.clone(),
            mime: reference.mime.clone(),
            coordinator_attempt_id: fence.coordinator_attempt_id.clone(),
            coordinator_epoch: fence.coordinator_epoch,
            created_at_ms: now,
        };
        put(db, session, "input_share", &share.id, child_id, &share)?;
    }
    Ok(())
}
pub(super) fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn create_request_digest(input: &CreateTask, refs: &[InputRef]) -> WorkResult<String> {
    validate_refs(refs)?;
    if refs.is_empty() {
        payload_digest(input)
    } else {
        payload_digest(&(input, refs))
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn delivery_request_digest(
    task: &str,
    attempt: &str,
    revision: u64,
    instance: &str,
    text: &str,
    version: Option<&str>,
    refs: &[InputRef],
) -> WorkResult<String> {
    validate_refs(refs)?;
    payload_digest(&(task, attempt, revision, instance, text, version, refs))
}
pub(super) fn legacy_delivery_digest(
    task: &str,
    attempt: &str,
    revision: u64,
    instance: &str,
    text: &str,
    version: Option<&str>,
) -> WorkResult<String> {
    payload_digest(&(task, attempt, revision, instance, text, version))
}

fn validate_upload(input: &InputUpload) -> WorkResult<()> {
    if !Path::new(&input.repo_path).is_absolute()
        || !bounded(&input.repo_path, 4096)
        || input.name.is_empty()
        || input.name.chars().count() > 120
        || input
            .name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        || !bounded(&input.mime, 128)
        || input.mime.chars().any(char::is_control)
        || input.size_bytes > 10 * 1024 * 1024
        || input.sha256.len() != 64
        || !input
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    Ok(())
}
pub(super) fn validate_refs(refs: &[InputRef]) -> WorkResult<()> {
    let mut seen = std::collections::HashSet::new();
    if refs.len() > 9 {
        return Err(invalid());
    }
    for reference in refs {
        if uuid::Uuid::parse_str(&reference.input_id).is_err()
            || reference.caption.len() > 4096
            || reference.caption.contains('\0')
            || !seen.insert(&reference.input_id)
        {
            return Err(invalid());
        }
    }
    Ok(())
}
fn upload_request<'a>(
    actor: &'a str,
    session: &'a str,
    key: &'a str,
    input: &InputUpload,
) -> WorkResult<Request<'a>> {
    validate_upload(input)?;
    Ok(Request {
        actor,
        session,
        key,
        kind: "input_upload",
        digest: payload_digest(input)?,
    })
}
pub(super) fn resolve(
    db: &Connection,
    actor: &str,
    session: &str,
    repo: &str,
    task: Option<&str>,
    refs: &[InputRef],
    now: i64,
) -> WorkResult<Vec<FrozenInputRef>> {
    validate_refs(refs)?;
    refs.iter().map(|reference| {
        let row: Option<(String, Option<String>, String)> = db.query_row("SELECT actor,claimed_task,body FROM work_inputs WHERE id=?1 AND session=?2 AND repo_path=?3",params![reference.input_id,session,repo], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (owner, claimed, body) = row.ok_or(WorkError(FailureCode::ScopeMismatch))?;
        let receipt: InputReceipt = serde_json::from_str(&body)?;
        if let Some(claimed) = claimed {
            if task != Some(claimed.as_str()) {
                let target=task.ok_or(WorkError(FailureCode::ScopeMismatch))?;
                validate_share(db,session,target,&receipt,&reference.use_)?;
            }
        } else {
            if owner != actor { return Err(WorkError(FailureCode::ScopeMismatch)); }
            if now >= receipt.expires_at_ms { return Err(WorkError(FailureCode::InputExpired)); }
        }
        Ok(FrozenInputRef {input_id:receipt.input_id,caption:reference.caption.clone(),use_:reference.use_.clone(),name:receipt.name,mime:receipt.mime,size_bytes:receipt.size_bytes,sha256:receipt.sha256})
    }).collect()
}
pub(super) fn claim(
    db: &Connection,
    session: &str,
    task: &str,
    refs: &[FrozenInputRef],
) -> WorkResult<()> {
    for reference in refs {
        let changed = db.execute("UPDATE work_inputs SET claimed_task=?3 WHERE id=?1 AND session=?2 AND (claimed_task IS NULL OR claimed_task=?3)",params![reference.input_id,session,task])?;
        if changed != 1 {
            let body: String = db.query_row(
                "SELECT body FROM work_inputs WHERE id=?1 AND session=?2",
                params![reference.input_id, session],
                |row| row.get(0),
            )?;
            let receipt: InputReceipt = serde_json::from_str(&body)?;
            validate_share(db, session, task, &receipt, &reference.use_)?;
        }
    }
    Ok(())
}
/// SQL NULL means legacy metadata absent; JSON null means an explicitly stored no-version request.
/// Legacy followups omitted their offered version, so only the deployed v1, caller version,
/// and no-version forms may be tried. Every candidate still verifies the complete request hash.
pub(super) fn replay_versions(
    db: &Connection,
    actor: &str,
    session: &str,
    key: &str,
    current: Option<&str>,
) -> WorkResult<Vec<Option<String>>> {
    let body: Option<String> = db.query_row("SELECT body FROM receipts WHERE actor=?1 AND session=?2 AND kind='deliver_prompt' AND request_key=?3", params![actor,session,key], |row|row.get(0)).optional()?;
    let Some(body) = body else {
        return Ok(vec![current.map(str::to_owned)]);
    };
    let original: Operation = serde_json::from_str(&body)?;
    let metadata: Option<Option<String>> = db
        .query_row(
            "SELECT instruction_version FROM work_delivery_inputs WHERE operation_id=?1",
            [&original.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(Some(json)) = metadata {
        return Ok(vec![serde_json::from_str(&json)?]);
    }
    if original.bootstrap_version.is_some() {
        return Ok(vec![original.bootstrap_version]);
    }
    let mut candidates = vec![current.map(str::to_owned)];
    for candidate in [Some("result-reporting-v1".to_owned()), None] {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}
pub(super) fn replay_delivery_request(
    db: &Connection,
    actor: &str,
    session: &str,
    key: &str,
    expected: &str,
    legacy_expected: Option<&str>,
) -> WorkResult<Option<Mutation<Operation>>> {
    let receipt:Option<(String,String)> = db.query_row("SELECT body,digest FROM receipts WHERE actor=?1 AND session=?2 AND kind='deliver_prompt' AND request_key=?3",params![actor,session,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((body, original_digest)) = receipt else {
        return Ok(None);
    };
    let original: Operation = serde_json::from_str(&body)?;
    let stored: Option<String> = db
        .query_row(
            "SELECT request_digest FROM work_delivery_inputs WHERE operation_id=?1",
            [&original.id],
            |r| r.get(0),
        )
        .optional()?;
    let matches = match stored {
        Some(stored) => stored == expected,
        None => original.input_refs.is_empty() && legacy_expected == Some(original_digest.as_str()),
    };
    if !matches {
        return Err(WorkError(FailureCode::RequestKeyConflict));
    }
    Ok(Some(Mutation {
        value: load(db, session, "operation", &original.id)?,
        replayed: true,
    }))
}
impl WorkStore {
    pub fn replay_create_with_inputs(
        &self,
        actor: &str,
        session: &str,
        key: &str,
        input: &CreateTask,
        refs: &[InputRef],
    ) -> WorkResult<Option<Mutation<Task>>> {
        let request = Request {
            actor,
            session,
            key,
            kind: "create_task",
            digest: create_request_digest(input, refs)?,
        };
        Ok(request.replay(&self.connection)?.map(|value| Mutation {
            value,
            replayed: true,
        }))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn replay_delivery_with_inputs(
        &self,
        actor: &str,
        session: &str,
        task: &str,
        attempt: &str,
        key: &str,
        revision: u64,
        instance: &str,
        text: &str,
        version: Option<&str>,
        refs: &[InputRef],
    ) -> WorkResult<Option<Mutation<Operation>>> {
        if !bounded(actor, 256) || !bounded(session, 256) || !bounded(key, 128) {
            return Err(invalid());
        }
        for version in replay_versions(&self.connection, actor, session, key, version)? {
            let expected = delivery_request_digest(
                task,
                attempt,
                revision,
                instance,
                text,
                version.as_deref(),
                refs,
            )?;
            let legacy = if refs.is_empty() {
                Some(legacy_delivery_digest(
                    task,
                    attempt,
                    revision,
                    instance,
                    text,
                    version.as_deref(),
                )?)
            } else {
                None
            };
            match replay_delivery_request(
                &self.connection,
                actor,
                session,
                key,
                &expected,
                legacy.as_deref(),
            ) {
                Err(WorkError(FailureCode::RequestKeyConflict)) => continue,
                outcome => return outcome,
            }
        }
        Err(WorkError(FailureCode::RequestKeyConflict))
    }

    pub fn replay_input_upload(
        &self,
        actor: &str,
        session: &str,
        key: &str,
        input: &InputUpload,
    ) -> WorkResult<Option<InputReceipt>> {
        upload_request(actor, session, key, input)?.replay(&self.connection)
    }
    pub fn commit_input_upload(
        &mut self,
        actor: &str,
        session: &str,
        key: &str,
        input: InputUpload,
        now: i64,
    ) -> WorkResult<InputReceipt> {
        let request = upload_request(actor, session, key, &input)?;
        let tx = self.connection.transaction()?;
        if let Some(receipt) = request.replay(&tx)? {
            return Ok(receipt);
        }
        let count: u64 = tx.query_row("SELECT COUNT(*) FROM work_inputs", [], |r| r.get(0))?;
        if count >= MAX_INPUTS {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        let receipt = InputReceipt {
            input_id: id(),
            session_id: session.into(),
            repo_path: input.repo_path,
            name: input.name,
            mime: input.mime,
            size_bytes: input.size_bytes,
            sha256: input.sha256,
            created_at_ms: now,
            expires_at_ms: now.checked_add(INPUT_TTL_MS).ok_or_else(invalid)?,
        };
        tx.execute("INSERT INTO work_inputs(id,actor,session,repo_path,claimed_task,body) VALUES(?1,?2,?3,?4,NULL,?5)",params![receipt.input_id,actor,session,receipt.repo_path,encoded(&receipt)?])?;
        request.save(&tx, &receipt)?;
        tx.commit()?;
        Ok(receipt)
    }
    pub fn get_input_receipt(
        &self,
        actor: &str,
        session: &str,
        key: &str,
    ) -> WorkResult<InputReceipt> {
        if !bounded(actor, 256) || !bounded(session, 256) || !bounded(key, 128) {
            return Err(invalid());
        }
        let body:Option<String> = self.connection.query_row("SELECT body FROM receipts WHERE actor=?1 AND session=?2 AND kind='input_upload' AND request_key=?3",params![actor,session,key],|r|r.get(0)).optional()?;
        serde_json::from_str(&body.ok_or(WorkError(FailureCode::NotFound))?).map_err(Into::into)
    }
    /// Read-only preflight; creation/delivery must revalidate and claim in their transaction.
    pub fn resolve_inputs(
        &self,
        actor: &str,
        session: &str,
        repo: &str,
        task: Option<&str>,
        refs: &[InputRef],
        now: i64,
    ) -> WorkResult<Vec<FrozenInputRef>> {
        if let Some(task_id) = task {
            let task: Task = load(&self.connection, session, "task", task_id)?;
            if task.repo_path != repo {
                return Err(WorkError(FailureCode::ScopeMismatch));
            }
        }
        resolve(&self.connection, actor, session, repo, task, refs, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn upload(name: &str) -> InputUpload {
        InputUpload {
            repo_path: "/project".into(),
            name: name.into(),
            mime: "text/plain".into(),
            size_bytes: 10,
            sha256: "a".repeat(64),
        }
    }
    fn task() -> CreateTask {
        CreateTask {
            repo_path: "/project".into(),
            title: "Task".into(),
            brief: "Work".into(),
            parent_task_id: None,
            policy: TaskPolicy {
                allowed_agents: vec!["codex".into()],
                max_workers: 0,
            },
        }
    }
    fn reference(receipt: &InputReceipt) -> InputRef {
        InputRef {
            input_id: receipt.input_id.clone(),
            caption: "Quotes \" and $() are data".into(),
            use_: InputUse::ReferenceOnly,
        }
    }
    #[test]
    fn scoped_upload_receipts_keep_original_identity_and_expiry() {
        let mut store = WorkStore::in_memory().unwrap();
        let first = store
            .commit_input_upload("actor", "session", "key", upload("file.txt"), 100)
            .unwrap();
        assert_eq!(first.expires_at_ms, 100 + INPUT_TTL_MS);
        assert_eq!(
            store
                .commit_input_upload(
                    "actor",
                    "session",
                    "key",
                    upload("file.txt"),
                    first.expires_at_ms + 100
                )
                .unwrap(),
            first
        );
        assert_eq!(
            store
                .get_input_receipt("other", "session", "key")
                .unwrap_err()
                .0,
            FailureCode::NotFound
        );
        assert_eq!(
            store
                .get_input_receipt("actor", "other", "key")
                .unwrap_err()
                .0,
            FailureCode::NotFound
        );
        assert_eq!(
            store
                .replay_input_upload("actor", "session", "key", &upload("different.txt"))
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
        let mut oversized = upload("big.txt");
        oversized.size_bytes = 10 * 1024 * 1024 + 1;
        assert_eq!(
            store
                .commit_input_upload("actor", "session", "big", oversized, 100)
                .unwrap_err()
                .0,
            FailureCode::InvalidInput
        );
        assert!(store
            .resolve_inputs(
                "actor",
                "session",
                "/project",
                None,
                &[reference(&first)],
                99
            )
            .is_ok());
        assert_eq!(
            store
                .resolve_inputs(
                    "actor",
                    "session",
                    "/project",
                    None,
                    &[reference(&first)],
                    first.expires_at_ms
                )
                .unwrap_err()
                .0,
            FailureCode::InputExpired
        );
    }
    #[test]
    fn create_claims_are_atomic_ordered_and_replay_after_expiry() {
        let mut store = WorkStore::in_memory().unwrap();
        let own = store
            .commit_input_upload("actor", "session", "one", upload("one.txt"), 0)
            .unwrap();
        let foreign = store
            .commit_input_upload("other", "session", "two", upload("two.txt"), 0)
            .unwrap();
        let refs = vec![reference(&own), reference(&foreign)];
        assert_eq!(
            store
                .create_task_with_inputs("actor", "session", "bad", task(), refs, 1)
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert!(store.list_tasks("session", None, 100).unwrap().is_empty());
        assert!(store
            .resolve_inputs("actor", "session", "/project", None, &[reference(&own)], 1)
            .is_ok());
        let second = store
            .commit_input_upload("actor", "session", "three", upload("three.txt"), 0)
            .unwrap();
        let refs = vec![reference(&second), reference(&own)];
        let created = store
            .create_task_with_inputs("actor", "session", "create", task(), refs.clone(), 1)
            .unwrap()
            .value;
        assert_eq!(
            created
                .input_refs
                .iter()
                .map(|r| r.input_id.clone())
                .collect::<Vec<_>>(),
            vec![second.input_id, own.input_id]
        );
        assert_eq!(
            store
                .create_task_with_inputs(
                    "actor",
                    "session",
                    "create",
                    task(),
                    refs.clone(),
                    INPUT_TTL_MS + 1
                )
                .unwrap()
                .value
                .id,
            created.id
        );
        assert!(store
            .replay_create_with_inputs("actor", "session", "create", &task(), &refs)
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .create_task_with_inputs("actor", "session", "steal", task(), refs.clone(), 2)
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert!(store
            .resolve_inputs(
                "other",
                "session",
                "/project",
                Some(&created.id),
                &refs,
                INPUT_TTL_MS + 1
            )
            .is_ok());
        assert_eq!(
            store
                .resolve_inputs(
                    "actor",
                    "session",
                    "/elsewhere",
                    Some(&created.id),
                    &refs,
                    2
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        let mut changed = refs;
        changed.reverse();
        assert_eq!(
            store
                .replay_create_with_inputs("actor", "session", "create", &task(), &changed)
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
    }
    #[test]
    fn delivery_freezes_only_selected_refs_and_replays_without_recapture() {
        let mut store = WorkStore::in_memory().unwrap();
        let initial = store
            .commit_input_upload("actor", "session", "initial", upload("initial.txt"), 0)
            .unwrap();
        let created = store
            .create_task_with_inputs(
                "actor",
                "session",
                "create",
                task(),
                vec![reference(&initial)],
                1,
            )
            .unwrap()
            .value;
        let start = store
            .prepare_attempt(
                "actor",
                "session",
                &created.id,
                "start",
                created.revision,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "launch",
                2,
            )
            .unwrap()
            .value;
        store.begin_operation("session", &start.id, 3).unwrap();
        store.claim_start_dispatch("session", &start.id, 4).unwrap();
        store
            .confirm_start_launch(
                "session",
                &start.id,
                NativeBinding {
                    instance_id: Some("launch".into()),
                    ..Default::default()
                },
                "epoch",
                5,
            )
            .unwrap();
        store
            .finalize_operation(
                "session",
                &start.id,
                OperationOutcome {
                    state: OperationState::Acknowledged,
                    resources: Default::default(),
                    failure_code: None,
                },
                6,
            )
            .unwrap();
        let fresh = store
            .commit_input_upload("actor", "session", "fresh", upload("fresh.txt"), 7)
            .unwrap();
        let refs = vec![reference(&fresh)];
        let frozen = store
            .resolve_inputs("actor", "session", "/project", Some(&created.id), &refs, 8)
            .unwrap();
        let revision = store.detail("session", &created.id).unwrap().task.revision;
        let aid = start.attempt_id.as_deref().unwrap();
        let mut bad = frozen.clone();
        bad[0].sha256 = "b".repeat(64);
        assert_eq!(
            store
                .prepare_delivery_with_inputs(
                    "actor",
                    "session",
                    &created.id,
                    aid,
                    "deliver",
                    revision,
                    "launch",
                    "User text",
                    Some("v1"),
                    refs.clone(),
                    &bad,
                    &"c".repeat(64),
                    8
                )
                .unwrap_err()
                .0,
            FailureCode::ArtifactChanged
        );
        assert!(store
            .resolve_inputs("actor", "session", "/project", None, &refs, 8)
            .is_ok());
        let delivery = store
            .prepare_delivery_with_inputs(
                "actor",
                "session",
                &created.id,
                aid,
                "deliver",
                revision,
                "launch",
                "User text",
                Some("v1"),
                refs.clone(),
                &frozen,
                &"c".repeat(64),
                8,
            )
            .unwrap()
            .value;
        assert_eq!(delivery.input_refs, frozen);
        assert_eq!(delivery.input_refs.len(), 1);
        assert_ne!(delivery.input_refs[0].input_id, initial.input_id);
        assert!(store
            .resolve_inputs("actor", "session", "/project", None, &refs, 8)
            .is_err());
        let replay = store
            .replay_delivery_with_inputs(
                "actor",
                "session",
                &created.id,
                aid,
                "deliver",
                revision,
                "launch",
                "User text",
                Some("v1"),
                &refs,
            )
            .unwrap()
            .unwrap();
        assert_eq!(replay.value.id, delivery.id);
        // The server's rendered prompt can change after an executable upgrade;
        // the same client request keeps its original frozen receipt instead.
        assert_eq!(
            store
                .prepare_delivery_with_inputs(
                    "actor",
                    "session",
                    &created.id,
                    aid,
                    "deliver",
                    revision,
                    "launch",
                    "User text",
                    Some("v1"),
                    refs.clone(),
                    &frozen,
                    &"d".repeat(64),
                    INPUT_TTL_MS + 1
                )
                .unwrap()
                .value
                .id,
            delivery.id
        );
        let mut changed = refs;
        changed[0].caption = "Changed".into();
        assert_eq!(
            store
                .replay_delivery_with_inputs(
                    "actor",
                    "session",
                    &created.id,
                    aid,
                    "deliver",
                    revision,
                    "launch",
                    "User text",
                    Some("v1"),
                    &changed
                )
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
    }
    #[test]
    fn input_limits_and_two_task_claim_race_preserve_one_owner() {
        use std::sync::{Arc, Barrier, Mutex};
        let mut store = WorkStore::in_memory().unwrap();
        let receipt = store
            .commit_input_upload("actor", "session", "upload", upload("one.txt"), 0)
            .unwrap();
        assert_eq!(
            validate_refs(&vec![reference(&receipt); 10]).unwrap_err().0,
            FailureCode::InvalidInput
        );
        assert_eq!(
            validate_refs(&vec![reference(&receipt); 2]).unwrap_err().0,
            FailureCode::InvalidInput
        );
        let store = Arc::new(Mutex::new(store));
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                store.lock().unwrap().create_task_with_inputs(
                    "actor",
                    "session",
                    "task-a",
                    task(),
                    vec![reference(&receipt)],
                    1,
                )
            });
            let b = scope.spawn(|| {
                barrier.wait();
                store.lock().unwrap().create_task_with_inputs(
                    "actor",
                    "session",
                    "task-b",
                    task(),
                    vec![reference(&receipt)],
                    1,
                )
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(results.0.is_ok(), results.1.is_ok());
        assert_eq!(
            store
                .lock()
                .unwrap()
                .list_tasks("session", None, 100)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn input_receipts_and_claims_survive_restart_and_v1_records_default_empty() {
        let dir = std::env::temp_dir().join(format!("muqun-inputs-{}", uuid::Uuid::new_v4()));
        let path = dir.join("work.sqlite");
        let (receipt, created) = {
            let mut store = WorkStore::open(&path).unwrap();
            let receipt = store
                .commit_input_upload("actor", "session", "upload", upload("one.txt"), 1)
                .unwrap();
            let created = store
                .create_task_with_inputs(
                    "actor",
                    "session",
                    "task",
                    task(),
                    vec![reference(&receipt)],
                    2,
                )
                .unwrap()
                .value;
            (receipt, created)
        };
        let mut store = WorkStore::open(&path).unwrap();
        assert_eq!(
            store
                .get_input_receipt("actor", "session", "upload")
                .unwrap(),
            receipt
        );
        assert_eq!(
            store
                .detail("session", &created.id)
                .unwrap()
                .task
                .input_refs,
            created.input_refs
        );
        assert!(store
            .resolve_inputs(
                "actor",
                "session",
                "/project",
                Some(&created.id),
                &[reference(&receipt)],
                INPUT_TTL_MS + 5
            )
            .is_ok());
        let mut old = serde_json::to_value(&created).unwrap();
        old.as_object_mut().unwrap().remove("input_refs");
        assert!(serde_json::from_value::<Task>(old)
            .unwrap()
            .input_refs
            .is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn migration_from_v1_preserves_text_only_creation_receipt() {
        let dir =
            std::env::temp_dir().join(format!("muqun-input-migration-{}", uuid::Uuid::new_v4()));
        let path = dir.join("work.sqlite");
        let original = {
            let mut store = WorkStore::open(&path).unwrap();
            let task = store
                .create_task("actor", "session", "old", task(), 1)
                .unwrap()
                .value;
            store.connection.execute_batch("DROP TABLE work_delivery_inputs; DROP TABLE work_inputs; UPDATE records SET body=json_remove(body,'$.input_refs') WHERE kind='task'; PRAGMA user_version=1;").unwrap();
            task
        };
        let mut store = WorkStore::open(&path).unwrap();
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(store
            .detail("session", &original.id)
            .unwrap()
            .task
            .input_refs
            .is_empty());
        assert_eq!(
            store
                .create_task("actor", "session", "old", task(), 2)
                .unwrap()
                .value
                .id,
            original.id
        );
        assert!(store
            .commit_input_upload("actor", "session", "new", upload("file.txt"), 3)
            .is_ok());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn migration_from_v1_replays_exact_bootstrap_delivery_only() {
        let dir =
            std::env::temp_dir().join(format!("muqun-delivery-migration-{}", uuid::Uuid::new_v4()));
        let path = dir.join("work.sqlite");
        let (task_id, attempt_id, revision, delivery_id) = {
            let mut store = WorkStore::open(&path).unwrap();
            let task = store
                .create_task("actor", "session", "task", task(), 1)
                .unwrap()
                .value;
            let start = store
                .prepare_attempt(
                    "actor",
                    "session",
                    &task.id,
                    "start",
                    task.revision,
                    NewAttempt {
                        agent_kind: "codex".into(),
                        role: AttemptRole::Lead,
                    },
                    "launch",
                    2,
                )
                .unwrap()
                .value;
            store.begin_operation("session", &start.id, 3).unwrap();
            store
                .finalize_operation(
                    "session",
                    &start.id,
                    OperationOutcome {
                        state: OperationState::Acknowledged,
                        resources: NativeBinding {
                            instance_id: Some("launch".into()),
                            ..Default::default()
                        },
                        failure_code: None,
                    },
                    4,
                )
                .unwrap();
            let revision = store.detail("session", &task.id).unwrap().task.revision;
            let aid = start.attempt_id.unwrap();
            let delivery = store
                .prepare_delivery_with_instructions(
                    "actor",
                    "session",
                    &task.id,
                    &aid,
                    "delivery",
                    revision,
                    "launch",
                    "Original text",
                    Some("onboarding-v1"),
                    5,
                )
                .unwrap()
                .value;
            assert_eq!(delivery.bootstrap_version.as_deref(), Some("onboarding-v1"));
            store.begin_operation("session", &delivery.id, 6).unwrap();
            store.connection.execute_batch("DROP TABLE work_delivery_inputs; DROP TABLE work_inputs; UPDATE records SET body=json_remove(body,'$.input_refs'); UPDATE receipts SET body=json_remove(body,'$.input_refs'); PRAGMA user_version=1;").unwrap();
            (task.id, aid, revision, delivery.id)
        };
        let mut store = WorkStore::open(&path).unwrap();
        let replay = store
            .replay_delivery_with_inputs(
                "actor",
                "session",
                &task_id,
                &attempt_id,
                "delivery",
                revision,
                "launch",
                "Original text",
                Some("onboarding-v1"),
                &[],
            )
            .unwrap()
            .unwrap();
        assert_eq!(replay.value.id, delivery_id);
        assert_eq!(replay.value.state, OperationState::Unconfirmed);
        assert_eq!(
            replay.value.bootstrap_version.as_deref(),
            Some("onboarding-v1")
        );
        assert_eq!(
            store
                .prepare_delivery_with_inputs(
                    "actor",
                    "session",
                    &task_id,
                    &attempt_id,
                    "delivery",
                    revision,
                    "launch",
                    "Original text",
                    Some("onboarding-v1"),
                    vec![],
                    &[],
                    &"a".repeat(64),
                    10
                )
                .unwrap()
                .value
                .id,
            delivery_id
        );
        for (text, instance, rev, version) in [
            ("Changed", "launch", revision, Some("onboarding-v1")),
            (
                "Original text",
                "other-launch",
                revision,
                Some("onboarding-v1"),
            ),
            (
                "Original text",
                "launch",
                revision + 1,
                Some("onboarding-v1"),
            ),
        ] {
            assert_eq!(
                store
                    .replay_delivery_with_inputs(
                        "actor",
                        "session",
                        &task_id,
                        &attempt_id,
                        "delivery",
                        rev,
                        instance,
                        text,
                        version,
                        &[]
                    )
                    .unwrap_err()
                    .0,
                FailureCode::RequestKeyConflict
            );
        }
        assert!(store
            .replay_delivery_with_inputs(
                "other-actor",
                "session",
                &task_id,
                &attempt_id,
                "delivery",
                revision,
                "launch",
                "Original text",
                Some("onboarding-v1"),
                &[]
            )
            .unwrap()
            .is_none());
        assert!(store
            .replay_delivery_with_inputs(
                "actor",
                "other-session",
                &task_id,
                &attempt_id,
                "delivery",
                revision,
                "launch",
                "Original text",
                Some("onboarding-v1"),
                &[]
            )
            .unwrap()
            .is_none());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn modern_empty_delivery_missing_sidecar_is_not_accepted_as_legacy() {
        let mut store = WorkStore::in_memory().unwrap();
        let task = store
            .create_task("actor", "session", "task", task(), 1)
            .unwrap()
            .value;
        let start = store
            .prepare_attempt(
                "actor",
                "session",
                &task.id,
                "start",
                task.revision,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "launch",
                2,
            )
            .unwrap()
            .value;
        store.begin_operation("session", &start.id, 3).unwrap();
        store
            .finalize_operation(
                "session",
                &start.id,
                OperationOutcome {
                    state: OperationState::Acknowledged,
                    resources: NativeBinding {
                        instance_id: Some("launch".into()),
                        ..Default::default()
                    },
                    failure_code: None,
                },
                4,
            )
            .unwrap();
        let revision = store.detail("session", &task.id).unwrap().task.revision;
        let aid = start.attempt_id.unwrap();
        let delivery = store
            .prepare_delivery_with_inputs(
                "actor",
                "session",
                &task.id,
                &aid,
                "delivery",
                revision,
                "launch",
                "Original text",
                Some("v1"),
                vec![],
                &[],
                &"a".repeat(64),
                5,
            )
            .unwrap()
            .value;
        store
            .connection
            .execute(
                "DELETE FROM work_delivery_inputs WHERE operation_id=?1",
                [delivery.id],
            )
            .unwrap();
        assert_eq!(
            store
                .replay_delivery_with_inputs(
                    "actor",
                    "session",
                    &task.id,
                    &aid,
                    "delivery",
                    revision,
                    "launch",
                    "Original text",
                    Some("v1"),
                    &[]
                )
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
    }
    #[test]
    fn instruction_version_replay_migrates_initial_followup_and_explicit_null() {
        let dir = std::env::temp_dir().join(format!("version-{}", id()));
        let path = dir.join("work.sqlite");
        for first in [false, true] {
            for legacy in [false, true] {
                for original_version in [None, Some("result-reporting-v1")] {
                    let mut store = WorkStore::open(&path).unwrap();
                    let task = store
                        .create_task("actor", "session", &id(), task(), 1)
                        .unwrap()
                        .value;
                    let key = id();
                    let mut op = new_operation(
                        &task.id,
                        Some("attempt".into()),
                        OperationKind::DeliverPrompt,
                        2,
                    );
                    op.bootstrap_version = if first {
                        original_version.map(str::to_owned)
                    } else {
                        None
                    };
                    let digest = delivery_request_digest(
                        &task.id,
                        "attempt",
                        7,
                        "launch",
                        "text",
                        original_version,
                        &[],
                    )
                    .unwrap();
                    put(
                        &store.connection,
                        "session",
                        "operation",
                        &op.id,
                        &task.id,
                        &op,
                    )
                    .unwrap();
                    Request {
                        actor: "actor",
                        session: "session",
                        key: &key,
                        kind: "deliver_prompt",
                        digest: "final-native-envelope".into(),
                    }
                    .save(&store.connection, &op)
                    .unwrap();
                    store.connection.execute("INSERT INTO work_delivery_inputs(operation_id,request_digest,final_prompt_digest,instruction_version) VALUES(?1,?2,'final',?3)",params![op.id,digest,if legacy {None}else{Some(encoded(&original_version).unwrap())}]).unwrap();
                    drop(store);
                    let store = WorkStore::open(&path).unwrap();
                    assert_eq!(
                        store
                            .replay_delivery_with_inputs(
                                "actor",
                                "session",
                                &task.id,
                                "attempt",
                                &key,
                                7,
                                "launch",
                                "text",
                                Some("managed-delegation-v2"),
                                &[]
                            )
                            .unwrap()
                            .unwrap()
                            .value
                            .id,
                        op.id
                    );
                    for (text, launch, rev, refs) in [
                        ("changed", "launch", 7, vec![]),
                        ("text", "other", 7, vec![]),
                        ("text", "launch", 8, vec![]),
                        (
                            "text",
                            "launch",
                            7,
                            vec![InputRef {
                                input_id: id(),
                                caption: "".into(),
                                use_: InputUse::ReferenceOnly,
                            }],
                        ),
                    ] {
                        assert_eq!(
                            store
                                .replay_delivery_with_inputs(
                                    "actor",
                                    "session",
                                    &task.id,
                                    "attempt",
                                    &key,
                                    rev,
                                    launch,
                                    text,
                                    Some("managed-delegation-v2"),
                                    &refs
                                )
                                .unwrap_err()
                                .0,
                            FailureCode::RequestKeyConflict
                        );
                    }
                    assert!(store
                        .replay_delivery_with_inputs(
                            "foreign",
                            "session",
                            &task.id,
                            "attempt",
                            &key,
                            7,
                            "launch",
                            "text",
                            Some("managed-delegation-v2"),
                            &[]
                        )
                        .unwrap()
                        .is_none());
                    if !legacy {
                        assert_eq!(
                            replay_versions(
                                &store.connection,
                                "actor",
                                "session",
                                &key,
                                Some("future-v3")
                            )
                            .unwrap(),
                            vec![original_version.map(str::to_owned)]
                        );
                    }
                }
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
