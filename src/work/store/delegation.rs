use super::*;

#[derive(Serialize)]
pub(super) struct DelegatedCreate {
    pub fence: DelegationFence,
    pub expected_parent_revision: u64,
    pub dependencies: Vec<TaskDependency>,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct DelegatedOrigin {
    pub actor_id: String,
    pub fence: DelegationFence,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct DependencyOrigin {
    actor_id: String,
    request_key: String,
    fence: DelegationFence,
}
pub(super) fn replay_delivery(
    db: &Connection,
    actor: &str,
    session: &str,
    key: &str,
    digest: &str,
    fence: &DelegationFence,
) -> WorkResult<Option<Mutation<Operation>>> {
    authorize_controller(db, session, fence)?;
    let replay = inputs::replay_delivery_request(db, actor, session, key, digest, None)?;
    if let Some(op) = &replay {
        if op.value.delegation_fence.as_ref() != Some(fence) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        authorize_child(
            db,
            session,
            fence,
            &load(db, session, "task", &op.value.task_id)?,
        )?;
    }
    Ok(replay)
}
pub(super) struct OperationTarget {
    pub task_id: String,
    pub attempt_id: String,
    pub kind: OperationKind,
    pub fence: DelegationFence,
}
pub(super) fn validate_operation_target(
    db: &Connection,
    session: &str,
    op: &Operation,
    target: &OperationTarget,
) -> WorkResult<()> {
    if !matches!(
        target.kind,
        OperationKind::StartAttempt | OperationKind::DeliverPrompt
    ) || op.kind != target.kind
        || op.task_id != target.task_id
        || op.attempt_id.as_deref() != Some(&target.attempt_id)
        || op.delegation_fence.as_ref() != Some(&target.fence)
    {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    authorize_child(
        db,
        session,
        &target.fence,
        &load(db, session, "task", &target.task_id)?,
    )
}
fn check_policy(policy: &DelegationPolicy) -> WorkResult<()> {
    if policy.max_children > 64 || policy.max_depth > 1 {
        return Err(invalid());
    }
    Ok(())
}
pub(super) fn authorize_controller(
    db: &Connection,
    session: &str,
    fence: &DelegationFence,
) -> WorkResult<Task> {
    let task: Task = load(db, session, "task", &fence.coordinator_task_id)?;
    if !task.delegation.policy.enabled
        || task.parent_task_id.is_some()
        || task.delegation.coordinator_epoch != fence.coordinator_epoch
        || task.delegation.coordinator_attempt_id.as_deref() != Some(&fence.coordinator_attempt_id)
    {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    let attempt: Attempt = load(db, session, "attempt", &fence.coordinator_attempt_id)?;
    if attempt.task_id != task.id
        || attempt.role != AttemptRole::Lead
        || !reservation_active(&attempt)
    {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    match_native_binding(&attempt, &fence.instance_id, &fence.native_owner_epoch)?;
    Ok(task)
}
pub(super) fn authorize_child(
    db: &Connection,
    session: &str,
    fence: &DelegationFence,
    child: &Task,
) -> WorkResult<()> {
    let parent = authorize_controller(db, session, fence)?;
    if child.parent_task_id.as_deref() != Some(&parent.id)
        || child.repo_path != parent.repo_path
        || child.policy.max_workers > parent.policy.max_workers
        || child
            .policy
            .allowed_agents
            .iter()
            .any(|kind| !parent.policy.allowed_agents.contains(kind))
    {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    Ok(())
}
pub(super) fn authorize_create(
    db: &Connection,
    session: &str,
    input: &CreateTask,
    context: &DelegatedCreate,
) -> WorkResult<()> {
    let parent = authorize_controller(db, session, &context.fence)?;
    launch_allowed(db, &parent)?;
    revision(&parent, context.expected_parent_revision)?;
    if input.parent_task_id.as_deref() != Some(&parent.id) || input.repo_path != parent.repo_path {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    if parent.delegation.policy.max_depth == 0 {
        return Err(WorkError(FailureCode::ResourceLimit));
    }
    let descendants:u64=db.query_row("WITH RECURSIVE tree(id) AS (SELECT ?2 UNION ALL SELECT r.id FROM records r JOIN tree ON json_extract(r.body,'$.parent_task_id')=tree.id WHERE r.session=?1 AND r.kind='task') SELECT COUNT(*)-1 FROM tree",params![session,parent.id],|r|r.get(0))?;
    if descendants >= u64::from(parent.delegation.policy.max_children) {
        return Err(WorkError(FailureCode::ResourceLimit));
    }
    Ok(())
}
pub(super) fn validate_dependencies(
    db: &Connection,
    session: &str,
    task: &Task,
    dependencies: &[TaskDependency],
) -> WorkResult<()> {
    if dependencies.len() > 16 {
        return Err(WorkError(FailureCode::ResourceLimit));
    }
    if dependencies.is_empty() {
        return Ok(());
    }
    if task.parent_task_id.is_none() {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    let mut seen = std::collections::HashSet::new();
    for dependency in dependencies {
        if !seen.insert(&dependency.prerequisite_task_id)
            || dependency.prerequisite_task_id == task.id
        {
            return Err(invalid());
        }
        let prerequisite: Task = load(db, session, "task", &dependency.prerequisite_task_id)?;
        if prerequisite.repo_path != task.repo_path
            || prerequisite.parent_task_id != task.parent_task_id
        {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        if let Some(submission_id) = &dependency.submission_id {
            let result: ResultSubmission = load(db, session, "result", submission_id)?;
            if result.task_id != prerequisite.id {
                return Err(WorkError(FailureCode::ScopeMismatch));
            }
        }
        let mut pending = vec![(prerequisite, 1)];
        let mut visited = std::collections::HashSet::new();
        while let Some((node, depth)) = pending.pop() {
            if node.id == task.id {
                return Err(invalid());
            }
            if depth > 16 || visited.len() > 1024 {
                return Err(WorkError(FailureCode::ResourceLimit));
            }
            if !visited.insert(node.id) {
                continue;
            }
            for edge in node.dependencies {
                pending.push((
                    load(db, session, "task", &edge.prerequisite_task_id)?,
                    depth + 1,
                ));
            }
        }
    }
    Ok(())
}
fn inspect_dependencies(
    db: &Connection,
    session: &str,
    task: &Task,
) -> WorkResult<(DependencyReadiness, Vec<DependencySnapshot>)> {
    if task.dependencies.is_empty() {
        return Ok((DependencyReadiness::Ready, Vec::new()));
    }
    validate_dependencies(db, session, task, &task.dependencies)?;
    let parent: Task = load(
        db,
        session,
        "task",
        task.parent_task_id.as_deref().ok_or_else(invalid)?,
    )?;
    let mut snapshot = Vec::new();
    for dependency in &task.dependencies {
        let Some(submission) = &dependency.submission_id else {
            return Ok((DependencyReadiness::WaitingForResult, Vec::new()));
        };
        let result: ResultSubmission = load(db, session, "result", submission)?;
        if result.task_id != dependency.prerequisite_task_id {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        let review_id = if parent.delegation.policy.dependency_requirement
            == DependencyRequirement::HumanAccepted
        {
            // records() orders by insertion rowid, not timestamps supplied by clocks.
            let reviews: Vec<Review> = records(db, session, "review", &result.task_id)?;
            let latest = reviews
                .into_iter()
                .rev()
                .find(|review| review.review.submission_id == *submission);
            match latest {
                Some(review) if review.review.decision == ReviewDecision::Accepted => {
                    Some(review.id)
                }
                _ => return Ok((DependencyReadiness::WaitingForAcceptance, Vec::new())),
            }
        } else {
            None
        };
        snapshot.push(DependencySnapshot {
            prerequisite_task_id: dependency.prerequisite_task_id.clone(),
            submission_id: submission.clone(),
            review_id,
        });
    }
    Ok((DependencyReadiness::Ready, snapshot))
}
pub(super) fn dependency_snapshot(
    db: &Connection,
    session: &str,
    task: &Task,
) -> WorkResult<Vec<DependencySnapshot>> {
    let (readiness, snapshot) = inspect_dependencies(db, session, task)?;
    if readiness != DependencyReadiness::Ready {
        return Err(WorkError(FailureCode::NotReady));
    }
    Ok(snapshot)
}
pub(super) fn resolve_child_inputs(
    db: &Connection,
    actor: &str,
    session: &str,
    fence: &DelegationFence,
    child: &Task,
    refs: &[InputRef],
    now: i64,
) -> WorkResult<Vec<FrozenInputRef>> {
    authorize_child(db, session, fence, child)?;
    for reference in refs {
        let initial = child
            .input_refs
            .iter()
            .find(|input| input.input_id == reference.input_id)
            .ok_or(WorkError(FailureCode::ScopeMismatch))?;
        if !inputs::use_allowed(&initial.use_, &reference.use_) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
    }
    inputs::resolve(
        db,
        actor,
        session,
        &child.repo_path,
        Some(&child.id),
        refs,
        now,
    )
}
impl WorkStore {
    pub fn resolve_delegated_inputs(
        &self,
        actor: &str,
        session: &str,
        fence: &DelegationFence,
        child_task: Option<&str>,
        refs: &[InputRef],
        now: i64,
    ) -> WorkResult<Vec<FrozenInputRef>> {
        let parent = authorize_controller(&self.connection, session, fence)?;
        match child_task {
            Some(task) => resolve_child_inputs(
                &self.connection,
                actor,
                session,
                fence,
                &load(&self.connection, session, "task", task)?,
                refs,
                now,
            ),
            None => {
                inputs::resolve_parent_shares(&self.connection, actor, session, &parent, refs, now)
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_delegated_delivery_with_inputs(
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
        fence: &DelegationFence,
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
            Some(fence.clone()),
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn replay_delegated_delivery(
        &self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        key: &str,
        revision: u64,
        instance: &str,
        text: &str,
        version: Option<&str>,
        refs: &[InputRef],
        fence: &DelegationFence,
    ) -> WorkResult<Option<Mutation<Operation>>> {
        if !bounded(actor, 256) || !bounded(session, 256) || !bounded(key, 128) {
            return Err(invalid());
        }
        for version in inputs::replay_versions(&self.connection, actor, session, key, version)? {
            let base = inputs::delivery_request_digest(
                task_id,
                attempt_id,
                revision,
                instance,
                text,
                version.as_deref(),
                refs,
            )?;
            match replay_delivery(
                &self.connection,
                actor,
                session,
                key,
                &payload_digest(&(base, fence))?,
                fence,
            ) {
                Err(WorkError(FailureCode::RequestKeyConflict)) => continue,
                outcome => return outcome,
            }
        }
        Err(WorkError(FailureCode::RequestKeyConflict))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn replay_delegated_create(
        &self,
        actor: &str,
        session: &str,
        key: &str,
        fence: &DelegationFence,
        parent_revision: u64,
        input: &CreateTask,
        dependencies: &[TaskDependency],
    ) -> WorkResult<Option<Mutation<Task>>> {
        self.replay_delegated_create_with_inputs(
            actor,
            session,
            key,
            fence,
            parent_revision,
            input,
            dependencies,
            &[],
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn replay_delegated_create_with_inputs(
        &self,
        actor: &str,
        session: &str,
        key: &str,
        fence: &DelegationFence,
        parent_revision: u64,
        input: &CreateTask,
        dependencies: &[TaskDependency],
        refs: &[InputRef],
    ) -> WorkResult<Option<Mutation<Task>>> {
        inputs::validate_refs(refs)?;
        authorize_controller(&self.connection, session, fence)?;
        let context = DelegatedCreate {
            fence: fence.clone(),
            expected_parent_revision: parent_revision,
            dependencies: dependencies.to_vec(),
        };
        let request = Request {
            actor,
            session,
            key,
            kind: "create_task",
            digest: payload_digest(&(input, refs, context))?,
        };
        let replay: Option<Task> = request.replay(&self.connection)?;
        if let Some(task) = &replay {
            authorize_child(&self.connection, session, fence, task)?;
        }
        Ok(replay.map(|value| Mutation {
            value,
            replayed: true,
        }))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn replay_delegated_attempt(
        &self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        revision: u64,
        input: &NewAttempt,
        digest: &str,
        fence: &DelegationFence,
    ) -> WorkResult<Option<Mutation<Operation>>> {
        authorize_child(
            &self.connection,
            session,
            fence,
            &load(&self.connection, session, "task", task_id)?,
        )?;
        let request = Request {
            actor,
            session,
            key,
            kind: "start_attempt",
            digest: payload_digest(&(task_id, revision, input, digest, Some(fence)))?,
        };
        let replay: Option<Operation> = request.replay(&self.connection)?;
        replay
            .map(|op| {
                Ok(Mutation {
                    value: load(&self.connection, session, "operation", &op.id)?,
                    replayed: true,
                })
            })
            .transpose()
    }
    pub fn get_delegated_operation(
        &self,
        session: &str,
        fence: &DelegationFence,
        operation_id: &str,
    ) -> WorkResult<Operation> {
        let op: Operation = load(&self.connection, session, "operation", operation_id)?;
        authorize_child(
            &self.connection,
            session,
            fence,
            &load(&self.connection, session, "task", &op.task_id)?,
        )?;
        Ok(op)
    }
    pub fn get_delegated_receipt(
        &mut self,
        actor: &str,
        session: &str,
        fence: &DelegationFence,
        kind: OperationKind,
        key: &str,
    ) -> WorkResult<RequestReceipt> {
        authorize_controller(&self.connection, session, fence)?;
        if !matches!(
            kind,
            OperationKind::CreateTask
                | OperationKind::StartAttempt
                | OperationKind::DeliverPrompt
                | OperationKind::SetDependencies
        ) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        let receipt = self.get_receipt(actor, session, kind.clone(), key)?;
        match kind {
            OperationKind::SetDependencies => {
                let task: Task = serde_json::from_value(receipt.value.clone())?;
                authorize_child(&self.connection, session, fence, &task)?;
                let origins: Vec<DependencyOrigin> =
                    records(&self.connection, session, "dependency_origin", &task.id)?;
                if !origins.iter().any(|origin| {
                    origin.actor_id == actor && origin.request_key == key && origin.fence == *fence
                }) {
                    return Err(WorkError(FailureCode::ScopeMismatch));
                }
            }
            OperationKind::CreateTask => {
                let task: Task = serde_json::from_value(receipt.value.clone())?;
                authorize_child(&self.connection, session, fence, &task)?;
                let origins: Vec<DelegatedOrigin> =
                    records(&self.connection, session, "delegated_origin", &task.id)?;
                if !origins
                    .iter()
                    .any(|origin| origin.actor_id == actor && origin.fence == *fence)
                {
                    return Err(WorkError(FailureCode::ScopeMismatch));
                }
            }
            _ => {
                let op: Operation = serde_json::from_value(receipt.value.clone())?;
                authorize_child(
                    &self.connection,
                    session,
                    fence,
                    &load(&self.connection, session, "task", &op.task_id)?,
                )?;
                if op.delegation_fence.as_ref() != Some(fence) {
                    return Err(WorkError(FailureCode::ScopeMismatch));
                }
            }
        }
        Ok(receipt)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn begin_delegated_operation(
        &mut self,
        session: &str,
        operation_id: &str,
        task_id: &str,
        attempt_id: &str,
        kind: OperationKind,
        fence: &DelegationFence,
        now: i64,
    ) -> WorkResult<Operation> {
        self.begin_operation_inner(
            session,
            operation_id,
            Some(OperationTarget {
                task_id: task_id.into(),
                attempt_id: attempt_id.into(),
                kind,
                fence: fence.clone(),
            }),
            now,
        )
    }
    pub fn claim_delegated_start_dispatch(
        &mut self,
        session: &str,
        operation_id: &str,
        task_id: &str,
        attempt_id: &str,
        fence: &DelegationFence,
        now: i64,
    ) -> WorkResult<Operation> {
        self.claim_start_dispatch_inner(
            session,
            operation_id,
            Some(OperationTarget {
                task_id: task_id.into(),
                attempt_id: attempt_id.into(),
                kind: OperationKind::StartAttempt,
                fence: fence.clone(),
            }),
            now,
        )
    }
    /// Parent-owned input claims cannot be reparented. An explicit sharing relation is required.
    #[allow(clippy::too_many_arguments)]
    pub fn create_delegated_task_with_inputs(
        &mut self,
        actor: &str,
        session: &str,
        key: &str,
        fence: &DelegationFence,
        expected_parent_revision: u64,
        input: CreateTask,
        dependencies: Vec<TaskDependency>,
        input_refs: Vec<InputRef>,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        self.create_task_inner(
            actor,
            session,
            key,
            input,
            input_refs,
            Some(DelegatedCreate {
                fence: fence.clone(),
                expected_parent_revision,
                dependencies,
            }),
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_delegated_delivery(
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
        fence: &DelegationFence,
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
            Some(fence.clone()),
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn configure_delegation(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: DelegationConfig,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        check_policy(&input.policy)?;
        let request = Request {
            actor,
            session,
            key,
            kind: "configure_delegation",
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
        if input.policy.enabled {
            if task.parent_task_id.is_some() {
                return Err(WorkError(FailureCode::CapabilityUnavailable));
            }
            let aid = input
                .coordinator_attempt_id
                .as_deref()
                .ok_or_else(invalid)?;
            let attempt: Attempt = load(&tx, session, "attempt", aid)?;
            if attempt.task_id != task_id
                || attempt.role != AttemptRole::Lead
                || !reservation_active(&attempt)
                || attempt.lifecycle.launch_phase != LaunchPhase::LaunchConfirmed
                || attempt.binding.instance_id.is_none()
                || attempt.lifecycle.native_owner_epoch.is_none()
            {
                return Err(WorkError(FailureCode::NotReady));
            }
        }
        task.delegation = DelegationState {
            policy: input.policy,
            coordinator_attempt_id: input.coordinator_attempt_id,
            coordinator_epoch: task
                .delegation
                .coordinator_epoch
                .checked_add(1)
                .ok_or_else(invalid)?,
        };
        changed(&tx, &mut task, "delegation_configured", task_id, now)?;
        request.save(&tx, &task)?;
        tx.commit()?;
        Ok(Mutation {
            value: task,
            replayed: false,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn create_delegated_task(
        &mut self,
        actor: &str,
        session: &str,
        key: &str,
        fence: &DelegationFence,
        expected_parent_revision: u64,
        input: CreateTask,
        dependencies: Vec<TaskDependency>,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        self.create_task_inner(
            actor,
            session,
            key,
            input,
            Vec::new(),
            Some(DelegatedCreate {
                fence: fence.clone(),
                expected_parent_revision,
                dependencies,
            }),
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_delegated_attempt(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        input: NewAttempt,
        digest: &str,
        fence: &DelegationFence,
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
            Some(fence.clone()),
            now,
        )
    }
    pub fn assert_delegation_authority(
        &self,
        session: &str,
        fence: &DelegationFence,
        child_task: Option<&str>,
    ) -> WorkResult<()> {
        match child_task {
            Some(task) => authorize_child(
                &self.connection,
                session,
                fence,
                &load(&self.connection, session, "task", task)?,
            ),
            None => authorize_controller(&self.connection, session, fence).map(|_| ()),
        }
    }
    pub fn dependency_readiness(
        &self,
        session: &str,
        task_id: &str,
    ) -> WorkResult<DependencyReadiness> {
        let task: Task = load(&self.connection, session, "task", task_id)?;
        inspect_dependencies(&self.connection, session, &task).map(|(readiness, _)| readiness)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn set_delegated_dependencies(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        dependencies: Vec<TaskDependency>,
        fence: &DelegationFence,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        self.set_dependencies(
            actor,
            session,
            task_id,
            key,
            expected_revision,
            dependencies,
            Some(fence),
            now,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn replay_delegated_dependencies(
        &self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        dependencies: &[TaskDependency],
        fence: &DelegationFence,
    ) -> WorkResult<Option<Mutation<Task>>> {
        authorize_child(
            &self.connection,
            session,
            fence,
            &load(&self.connection, session, "task", task_id)?,
        )?;
        let request = Request {
            actor,
            session,
            key,
            kind: "set_dependencies",
            digest: payload_digest(&(task_id, expected_revision, dependencies, Some(fence)))?,
        };
        Ok(request.replay(&self.connection)?.map(|value| Mutation {
            value,
            replayed: true,
        }))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn set_dependencies(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        key: &str,
        expected_revision: u64,
        dependencies: Vec<TaskDependency>,
        fence: Option<&DelegationFence>,
        now: i64,
    ) -> WorkResult<Mutation<Task>> {
        let request = Request {
            actor,
            session,
            key,
            kind: "set_dependencies",
            digest: payload_digest(&(task_id, expected_revision, &dependencies, &fence))?,
        };
        let tx = self.connection.transaction()?;
        if let Some(fence) = fence {
            authorize_child(&tx, session, fence, &load(&tx, session, "task", task_id)?)?;
        }
        if let Some(value) = request.replay(&tx)? {
            return Ok(Mutation {
                value,
                replayed: true,
            });
        }
        let mut task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, expected_revision)?;
        if let Some(fence) = fence {
            authorize_child(&tx, session, fence, &task)?;
        }
        let attempts: Vec<Attempt> = records(&tx, session, "attempt", task_id)?;
        if attempts
            .iter()
            .any(|attempt| attempt.lifecycle.launch_phase != LaunchPhase::NotDispatched)
        {
            return Err(WorkError(FailureCode::NotReady));
        }
        validate_dependencies(&tx, session, &task, &dependencies)?;
        if let Some(fence) = fence {
            put(
                &tx,
                session,
                "dependency_origin",
                &id(),
                task_id,
                &DependencyOrigin {
                    actor_id: actor.into(),
                    request_key: key.into(),
                    fence: fence.clone(),
                },
            )?;
        }
        task.dependencies = dependencies;
        changed(&tx, &mut task, "dependencies_updated", task_id, now)?;
        request.save(&tx, &task)?;
        tx.commit()?;
        Ok(Mutation {
            value: task,
            replayed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(parent: Option<String>) -> CreateTask {
        CreateTask {
            repo_path: "/project".into(),
            title: "Task".into(),
            brief: "Do work".into(),
            parent_task_id: parent,
            policy: TaskPolicy {
                allowed_agents: vec!["codex".into()],
                max_workers: 2,
            },
        }
    }
    fn revision(store: &mut WorkStore, id: &str) -> u64 {
        store.detail("s", id).unwrap().task.revision
    }
    fn fixture(
        requirement: DependencyRequirement,
        max_children: u32,
    ) -> (WorkStore, Task, DelegationFence) {
        fixture_inputs(requirement, max_children, false)
    }
    fn fixture_inputs(
        requirement: DependencyRequirement,
        max_children: u32,
        with_inputs: bool,
    ) -> (WorkStore, Task, DelegationFence) {
        fixture_store(
            WorkStore::in_memory().unwrap(),
            requirement,
            max_children,
            with_inputs,
        )
    }
    fn fixture_store(
        mut store: WorkStore,
        requirement: DependencyRequirement,
        max_children: u32,
        with_inputs: bool,
    ) -> (WorkStore, Task, DelegationFence) {
        let mut refs = Vec::new();
        if with_inputs {
            for (index, use_) in [InputUse::ReferenceOnly, InputUse::MayInclude]
                .into_iter()
                .enumerate()
            {
                let upload = store
                    .commit_input_upload(
                        "phone",
                        "s",
                        &format!("upload-{index}"),
                        InputUpload {
                            repo_path: "/project".into(),
                            name: format!("source-{index}.txt"),
                            mime: "text/plain".into(),
                            size_bytes: 5,
                            sha256: "a".repeat(64),
                        },
                        0,
                    )
                    .unwrap();
                refs.push(InputRef {
                    input_id: upload.input_id,
                    caption: "Parent reference".into(),
                    use_,
                });
            }
        }
        let root = store
            .create_task_with_inputs("phone", "s", "root", input(None), refs, 1)
            .unwrap()
            .value;
        assert!(!root.delegation.policy.enabled);
        let op = store
            .prepare_attempt(
                "phone",
                "s",
                &root.id,
                "lead",
                root.revision,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "launch",
                2,
            )
            .unwrap()
            .value;
        store.begin_operation("s", &op.id, 3).unwrap();
        store.claim_start_dispatch("s", &op.id, 4).unwrap();
        store
            .confirm_start_launch(
                "s",
                &op.id,
                NativeBinding {
                    instance_id: Some("root-launch".into()),
                    ..Default::default()
                },
                "owner",
                5,
            )
            .unwrap();
        store
            .finalize_operation(
                "s",
                &op.id,
                OperationOutcome {
                    state: OperationState::Acknowledged,
                    resources: Default::default(),
                    failure_code: None,
                },
                6,
            )
            .unwrap();
        let rev = revision(&mut store, &root.id);
        let root = store
            .configure_delegation(
                "phone",
                "s",
                &root.id,
                "enable",
                rev,
                DelegationConfig {
                    policy: DelegationPolicy {
                        enabled: true,
                        max_children,
                        max_depth: 1,
                        dependency_requirement: requirement,
                    },
                    coordinator_attempt_id: op.attempt_id.clone(),
                },
                7,
            )
            .unwrap()
            .value;
        let fence = DelegationFence {
            coordinator_task_id: root.id.clone(),
            coordinator_attempt_id: op.attempt_id.unwrap(),
            coordinator_epoch: root.delegation.coordinator_epoch,
            instance_id: "root-launch".into(),
            native_owner_epoch: "owner".into(),
        };
        (store, root, fence)
    }
    fn child(
        store: &mut WorkStore,
        root: &Task,
        fence: &DelegationFence,
        key: &str,
        deps: Vec<TaskDependency>,
    ) -> Task {
        let rev = revision(store, &root.id);
        store
            .create_delegated_task(
                "lead",
                "s",
                key,
                fence,
                rev,
                input(Some(root.id.clone())),
                deps,
                8,
            )
            .unwrap()
            .value
    }
    fn start(
        store: &mut WorkStore,
        child: &Task,
        fence: &DelegationFence,
        key: &str,
    ) -> WorkResult<Mutation<Operation>> {
        let rev = revision(store, &child.id);
        store.prepare_delegated_attempt(
            "lead",
            "s",
            &child.id,
            key,
            rev,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            fence,
            9,
        )
    }
    #[test]
    fn child_scope_epoch_pause_and_lifetime_budget_are_fenced() {
        let (mut store, root, fence) = fixture(DependencyRequirement::ResultAvailable, 1);
        let rev = revision(&mut store, &root.id);
        let mut forged = fence.clone();
        forged.instance_id = "replacement".into();
        assert_eq!(
            store
                .create_delegated_task(
                    "lead",
                    "s",
                    "forged",
                    &forged,
                    rev,
                    input(Some(root.id.clone())),
                    vec![],
                    8
                )
                .unwrap_err()
                .0,
            FailureCode::InstanceChanged
        );
        let first = child(&mut store, &root, &fence, "child", vec![]);
        assert!(!first.delegation.policy.enabled);
        assert_eq!(first.delegation.policy.max_depth, 0);
        assert_eq!(
            store
                .create_delegated_task(
                    "lead",
                    "s",
                    "child",
                    &fence,
                    rev,
                    input(Some(root.id.clone())),
                    vec![],
                    9
                )
                .unwrap()
                .value
                .id,
            first.id
        );
        let current = revision(&mut store, &root.id);
        assert_eq!(
            store
                .create_delegated_task(
                    "lead",
                    "s",
                    "over",
                    &fence,
                    current,
                    input(Some(root.id.clone())),
                    vec![],
                    10
                )
                .unwrap_err()
                .0,
            FailureCode::ResourceLimit
        );
        let prepared = start(&mut store, &first, &fence, "start").unwrap().value;
        store.begin_operation("s", &prepared.id, 11).unwrap();
        let rev = revision(&mut store, &root.id);
        store
            .set_paused(
                "phone",
                "s",
                &root.id,
                "pause",
                rev,
                PauseInput { paused: true },
                12,
            )
            .unwrap();
        assert_eq!(
            store
                .claim_start_dispatch("s", &prepared.id, 13)
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
        let rev = revision(&mut store, &root.id);
        store
            .set_paused(
                "phone",
                "s",
                &root.id,
                "resume",
                rev,
                PauseInput { paused: false },
                14,
            )
            .unwrap();
        let rev = revision(&mut store, &root.id);
        store
            .configure_delegation(
                "phone",
                "s",
                &root.id,
                "takeover",
                rev,
                DelegationConfig {
                    policy: root.delegation.policy.clone(),
                    coordinator_attempt_id: Some(fence.coordinator_attempt_id.clone()),
                },
                15,
            )
            .unwrap();
        assert_eq!(
            store
                .claim_start_dispatch("s", &prepared.id, 16)
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(
            store.detail("s", &first.id).unwrap().attempts[0]
                .lifecycle
                .reservation,
            Reservation::Reserved
        );
    }
    #[test]
    fn exact_dependency_result_and_review_insertion_order_gate_dispatch() {
        let (mut store, root, fence) = fixture(DependencyRequirement::HumanAccepted, 4);
        let prerequisite = child(&mut store, &root, &fence, "prerequisite", vec![]);
        let dependent = child(
            &mut store,
            &root,
            &fence,
            "dependent",
            vec![TaskDependency {
                prerequisite_task_id: prerequisite.id.clone(),
                submission_id: None,
            }],
        );
        assert_eq!(
            store.dependency_readiness("s", &dependent.id).unwrap(),
            DependencyReadiness::WaitingForResult
        );
        assert_eq!(
            start(&mut store, &dependent, &fence, "blocked")
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
        assert!(store
            .detail("s", &dependent.id)
            .unwrap()
            .attempts
            .is_empty());
        let attempt = start(&mut store, &prerequisite, &fence, "candidate")
            .unwrap()
            .value;
        let rev = revision(&mut store, &prerequisite.id);
        let result = store
            .submit_result(
                "worker",
                "s",
                &prerequisite.id,
                "result",
                rev,
                ResultInput {
                    attempt_id: attempt.attempt_id.unwrap(),
                    summary: "Candidate".into(),
                    artifacts: vec![],
                    evidence: vec![],
                },
                100,
            )
            .unwrap()
            .value;
        let rev = revision(&mut store, &dependent.id);
        store
            .set_delegated_dependencies(
                "lead",
                "s",
                &dependent.id,
                "bind",
                rev,
                vec![TaskDependency {
                    prerequisite_task_id: prerequisite.id.clone(),
                    submission_id: Some(result.id.clone()),
                }],
                &fence,
                101,
            )
            .unwrap();
        assert_eq!(
            store.dependency_readiness("s", &dependent.id).unwrap(),
            DependencyReadiness::WaitingForAcceptance
        );
        let rev = revision(&mut store, &prerequisite.id);
        store
            .review_result(
                "phone",
                "s",
                &prerequisite.id,
                "accept",
                rev,
                ReviewInput {
                    submission_id: result.id.clone(),
                    decision: ReviewDecision::Accepted,
                    message: None,
                },
                1000,
            )
            .unwrap();
        let op = start(&mut store, &dependent, &fence, "start")
            .unwrap()
            .value;
        assert_eq!(op.dependency_snapshot[0].submission_id, result.id);
        store.begin_operation("s", &op.id, 102).unwrap();
        let rev = revision(&mut store, &prerequisite.id);
        store
            .review_result(
                "phone",
                "s",
                &prerequisite.id,
                "changes",
                rev,
                ReviewInput {
                    submission_id: result.id.clone(),
                    decision: ReviewDecision::ChangesRequested,
                    message: None,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            store.claim_start_dispatch("s", &op.id, 103).unwrap_err().0,
            FailureCode::NotReady
        );
        let rev = revision(&mut store, &prerequisite.id);
        let accepted = store
            .review_result(
                "phone",
                "s",
                &prerequisite.id,
                "accept-again",
                rev,
                ReviewInput {
                    submission_id: result.id.clone(),
                    decision: ReviewDecision::Accepted,
                    message: None,
                },
                0,
            )
            .unwrap()
            .value;
        let claimed = store.claim_start_dispatch("s", &op.id, 104).unwrap();
        assert_eq!(
            claimed.dependency_snapshot[0].review_id.as_deref(),
            Some(accepted.id.as_str())
        );
        let rev = revision(&mut store, &dependent.id);
        assert_eq!(
            store
                .set_dependencies(
                    "lead",
                    "s",
                    &dependent.id,
                    "after-start",
                    rev,
                    vec![],
                    Some(&fence),
                    105
                )
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
    }
    #[test]
    fn dependencies_reject_cycles_foreign_results_and_recursive_delegation() {
        let (mut store, root, fence) = fixture(DependencyRequirement::ResultAvailable, 4);
        let a = child(&mut store, &root, &fence, "a", vec![]);
        let b = child(
            &mut store,
            &root,
            &fence,
            "b",
            vec![TaskDependency {
                prerequisite_task_id: a.id.clone(),
                submission_id: None,
            }],
        );
        let rev = revision(&mut store, &a.id);
        assert_eq!(
            store
                .set_dependencies(
                    "lead",
                    "s",
                    &a.id,
                    "cycle",
                    rev,
                    vec![TaskDependency {
                        prerequisite_task_id: b.id.clone(),
                        submission_id: None
                    }],
                    Some(&fence),
                    9
                )
                .unwrap_err()
                .0,
            FailureCode::InvalidInput
        );
        assert_eq!(
            store
                .set_dependencies(
                    "lead",
                    "s",
                    &a.id,
                    "parent",
                    rev,
                    vec![TaskDependency {
                        prerequisite_task_id: root.id.clone(),
                        submission_id: None
                    }],
                    Some(&fence),
                    10
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(
            store
                .configure_delegation(
                    "phone",
                    "s",
                    &a.id,
                    "recursive",
                    rev,
                    DelegationConfig {
                        policy: root.delegation.policy.clone(),
                        coordinator_attempt_id: Some(fence.coordinator_attempt_id)
                    },
                    11
                )
                .unwrap_err()
                .0,
            FailureCode::CapabilityUnavailable
        );
    }
    #[test]
    fn admitted_delegated_start_persists_dependency_evidence_at_record_cap() {
        let (mut store, root, fence) = fixture(DependencyRequirement::ResultAvailable, 4);
        let prerequisite = child(&mut store, &root, &fence, "prerequisite", vec![]);
        let attempt = start(&mut store, &prerequisite, &fence, "prerequisite-attempt")
            .unwrap()
            .value;
        let rev = revision(&mut store, &prerequisite.id);
        let result = store
            .submit_result(
                "worker",
                "s",
                &prerequisite.id,
                "candidate",
                rev,
                ResultInput {
                    attempt_id: attempt.attempt_id.clone().unwrap(),
                    summary: "Candidate".into(),
                    artifacts: vec![],
                    evidence: vec![],
                },
                10,
            )
            .unwrap()
            .value;
        let dependent = child(
            &mut store,
            &root,
            &fence,
            "dependent",
            vec![TaskDependency {
                prerequisite_task_id: prerequisite.id.clone(),
                submission_id: Some(result.id.clone()),
            }],
        );
        let tx = store.connection.transaction().unwrap();
        let existing: u64 = tx
            .query_row(
                "SELECT COUNT(*) FROM records WHERE task_id=?1",
                [&dependent.id],
                |row| row.get(0),
            )
            .unwrap();
        for index in existing..1021 {
            tx.execute("INSERT INTO records(id,session,kind,task_id,body) VALUES(?1,'s','reserved_test_history',?2,'{}')",params![format!("quota-{index}"),dependent.id]).unwrap();
        }
        tx.commit().unwrap();
        let op = start(&mut store, &dependent, &fence, "start")
            .unwrap()
            .value;
        store.begin_operation("s", &op.id, 11).unwrap();
        let claimed = store.claim_start_dispatch("s", &op.id, 12).unwrap();
        assert_eq!(claimed.dependency_snapshot[0].submission_id, result.id);
        store
            .record_start_refusal("s", &op.id, "owner", "no-process", 13)
            .unwrap();
        store
            .finalize_operation(
                "s",
                &op.id,
                OperationOutcome {
                    state: OperationState::Refused,
                    resources: Default::default(),
                    failure_code: Some(FailureCode::NotReady),
                },
                14,
            )
            .unwrap();
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM records WHERE task_id=?1",
                    [&dependent.id],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            1024
        );
        assert!(store.get_start_refusal("s", &op.id).unwrap().is_some());
    }
    #[test]
    fn unrelated_child_inputs_refuse_without_stealing_claims() {
        let (mut store, root, fence) = fixture(DependencyRequirement::ResultAvailable, 4);
        let rev = revision(&mut store, &root.id);
        assert_eq!(
            store
                .create_delegated_task_with_inputs(
                    "lead",
                    "s",
                    "child-inputs",
                    &fence,
                    rev,
                    input(Some(root.id.clone())),
                    vec![],
                    vec![InputRef {
                        input_id: uuid::Uuid::new_v4().to_string(),
                        caption: "Reference".into(),
                        use_: InputUse::ReferenceOnly
                    }],
                    8
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(store.list_tasks("s", None, 100).unwrap().len(), 1);
    }
    #[test]
    fn simultaneous_child_creation_consumes_one_parent_revision_and_slot() {
        use std::sync::{Arc, Barrier, Mutex};
        let (store, root, fence) = fixture(DependencyRequirement::ResultAvailable, 1);
        let store = Arc::new(Mutex::new(store));
        let barrier = Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                store.lock().unwrap().create_delegated_task(
                    "lead",
                    "s",
                    "a",
                    &fence,
                    root.revision,
                    input(Some(root.id.clone())),
                    vec![],
                    10,
                )
            });
            let b = scope.spawn(|| {
                barrier.wait();
                store.lock().unwrap().create_delegated_task(
                    "lead",
                    "s",
                    "b",
                    &fence,
                    root.revision,
                    input(Some(root.id.clone())),
                    vec![],
                    10,
                )
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(results.0.is_ok(), results.1.is_ok());
        assert_eq!(
            store
                .lock()
                .unwrap()
                .list_tasks("s", None, 100)
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn parent_input_shares_narrow_permissions_and_preserve_one_source_claim() {
        let (mut store, root, fence) =
            fixture_inputs(DependencyRequirement::ResultAvailable, 8, true);
        let refs = vec![InputRef {
            input_id: root.input_refs[0].input_id.clone(),
            caption: "Child reference".into(),
            use_: InputUse::ReferenceOnly,
        }];
        let parent_revision = revision(&mut store, &root.id);
        let first = store
            .create_delegated_task_with_inputs(
                "lead",
                "s",
                "first",
                &fence,
                parent_revision,
                input(Some(root.id.clone())),
                vec![],
                refs.clone(),
                10,
            )
            .unwrap()
            .value;
        let rev = revision(&mut store, &root.id);
        let second = store
            .create_delegated_task_with_inputs(
                "lead",
                "s",
                "second",
                &fence,
                rev,
                input(Some(root.id.clone())),
                vec![],
                refs.clone(),
                11,
            )
            .unwrap()
            .value;
        assert_ne!(first.id, second.id);
        assert_eq!(first.input_refs, second.input_refs);
        let claimed: String = store
            .connection
            .query_row(
                "SELECT claimed_task FROM work_inputs WHERE id=?1",
                [&refs[0].input_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(claimed, root.id);
        for child in [&first, &second] {
            let shares: Vec<InputShare> =
                records(&store.connection, "s", "input_share", &child.id).unwrap();
            assert_eq!(shares.len(), 1);
            assert_eq!(shares[0].source_task_id, root.id);
            assert!(store
                .resolve_inputs(
                    "phone",
                    "s",
                    "/project",
                    Some(&child.id),
                    &refs,
                    49 * 60 * 60 * 1000
                )
                .is_ok());
        }
        let mut wider = refs.clone();
        wider[0].use_ = InputUse::MayInclude;
        let rev = revision(&mut store, &root.id);
        assert_eq!(
            store
                .create_delegated_task_with_inputs(
                    "lead",
                    "s",
                    "widen",
                    &fence,
                    rev,
                    input(Some(root.id.clone())),
                    vec![],
                    wider.clone(),
                    12
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(
            store
                .resolve_delegated_inputs("lead", "s", &fence, Some(&first.id), &wider, 12)
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        let narrower = vec![InputRef {
            input_id: root.input_refs[1].input_id.clone(),
            caption: "Narrower".into(),
            use_: InputUse::ReferenceOnly,
        }];
        let third = store
            .create_delegated_task_with_inputs(
                "lead",
                "s",
                "third",
                &fence,
                rev,
                input(Some(root.id.clone())),
                vec![],
                narrower.clone(),
                13,
            )
            .unwrap()
            .value;
        assert_eq!(third.input_refs[0].use_, InputUse::ReferenceOnly);
        let mut invalid = refs.clone();
        invalid.push(InputRef {
            input_id: uuid::Uuid::new_v4().to_string(),
            caption: "Foreign".into(),
            use_: InputUse::ReferenceOnly,
        });
        let rev = revision(&mut store, &root.id);
        let before = store.list_tasks("s", None, 100).unwrap().len();
        assert_eq!(
            store
                .create_delegated_task_with_inputs(
                    "lead",
                    "s",
                    "invalid-second",
                    &fence,
                    rev,
                    input(Some(root.id.clone())),
                    vec![],
                    invalid,
                    14
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(store.list_tasks("s", None, 100).unwrap().len(), before);
        assert_eq!(revision(&mut store, &root.id), rev);
        assert_eq!(
            store
                .replay_delegated_create_with_inputs(
                    "lead",
                    "s",
                    "first",
                    &fence,
                    parent_revision,
                    &input(Some(root.id.clone())),
                    &[],
                    &refs
                )
                .unwrap()
                .unwrap()
                .value
                .id,
            first.id
        );
        assert_eq!(
            store
                .get_delegated_receipt("lead", "s", &fence, OperationKind::CreateTask, "first")
                .unwrap()
                .value["id"],
            first.id
        );
        assert_eq!(
            store
                .get_delegated_receipt("other", "s", &fence, OperationKind::CreateTask, "first")
                .unwrap_err()
                .0,
            FailureCode::NotFound
        );
    }
    #[test]
    fn delegated_final_digest_replay_closed_targets_and_takeover_history() {
        let (mut store, root, fence) =
            fixture_inputs(DependencyRequirement::ResultAvailable, 4, true);
        let refs = vec![InputRef {
            input_id: root.input_refs[0].input_id.clone(),
            caption: "Reference".into(),
            use_: InputUse::ReferenceOnly,
        }];
        let rev = revision(&mut store, &root.id);
        let child = store
            .create_delegated_task_with_inputs(
                "lead",
                "s",
                "child",
                &fence,
                rev,
                input(Some(root.id.clone())),
                vec![],
                refs.clone(),
                10,
            )
            .unwrap()
            .value;
        let op = start(&mut store, &child, &fence, "start").unwrap().value;
        let aid = op.attempt_id.as_deref().unwrap();
        assert_eq!(
            store
                .begin_delegated_operation(
                    "s",
                    &op.id,
                    &root.id,
                    aid,
                    OperationKind::StartAttempt,
                    &fence,
                    11
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(
            store
                .begin_delegated_operation(
                    "s",
                    &op.id,
                    &child.id,
                    aid,
                    OperationKind::DeliverPrompt,
                    &fence,
                    11
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        store
            .begin_delegated_operation(
                "s",
                &op.id,
                &child.id,
                aid,
                OperationKind::StartAttempt,
                &fence,
                12,
            )
            .unwrap();
        store
            .claim_delegated_start_dispatch("s", &op.id, &child.id, aid, &fence, 13)
            .unwrap();
        store
            .confirm_start_launch(
                "s",
                &op.id,
                NativeBinding {
                    instance_id: Some("child-launch".into()),
                    ..Default::default()
                },
                "child-owner",
                14,
            )
            .unwrap();
        store
            .finalize_operation(
                "s",
                &op.id,
                OperationOutcome {
                    state: OperationState::Acknowledged,
                    resources: Default::default(),
                    failure_code: None,
                },
                15,
            )
            .unwrap();
        let frozen = store
            .resolve_delegated_inputs("lead", "s", &fence, Some(&child.id), &refs, 16)
            .unwrap();
        let rev = revision(&mut store, &child.id);
        let delivery = store
            .prepare_delegated_delivery_with_inputs(
                "lead",
                "s",
                &child.id,
                aid,
                "send",
                rev,
                "child-launch",
                "User text",
                Some("v1"),
                refs.clone(),
                &frozen,
                &"c".repeat(64),
                &fence,
                16,
            )
            .unwrap()
            .value;
        assert_eq!(delivery.input_refs, frozen);
        assert_eq!(
            store
                .replay_delegated_delivery(
                    "lead",
                    "s",
                    &child.id,
                    aid,
                    "send",
                    rev,
                    "child-launch",
                    "User text",
                    Some("v1"),
                    &refs,
                    &fence
                )
                .unwrap()
                .unwrap()
                .value
                .id,
            delivery.id
        );
        assert_eq!(
            store
                .prepare_delegated_delivery_with_inputs(
                    "lead",
                    "s",
                    &child.id,
                    aid,
                    "send",
                    rev,
                    "child-launch",
                    "User text",
                    Some("v1"),
                    refs.clone(),
                    &frozen,
                    &"d".repeat(64),
                    &fence,
                    17
                )
                .unwrap()
                .value
                .id,
            delivery.id
        );
        assert_eq!(
            store
                .replay_delegated_delivery(
                    "lead",
                    "s",
                    &child.id,
                    aid,
                    "send",
                    rev,
                    "child-launch",
                    "Changed",
                    Some("v1"),
                    &refs,
                    &fence
                )
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
        let rev = revision(&mut store, &root.id);
        let configured = store
            .configure_delegation(
                "phone",
                "s",
                &root.id,
                "takeover",
                rev,
                DelegationConfig {
                    policy: root.delegation.policy.clone(),
                    coordinator_attempt_id: Some(fence.coordinator_attempt_id.clone()),
                },
                18,
            )
            .unwrap()
            .value;
        let mut successor = fence.clone();
        successor.coordinator_epoch = configured.delegation.coordinator_epoch;
        assert_eq!(
            store
                .get_delegated_operation("s", &successor, &delivery.id)
                .unwrap()
                .id,
            delivery.id
        );
        assert_eq!(
            store
                .get_delegated_receipt(
                    "lead",
                    "s",
                    &successor,
                    OperationKind::DeliverPrompt,
                    "send"
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(
            store
                .begin_delegated_operation(
                    "s",
                    &delivery.id,
                    &child.id,
                    aid,
                    OperationKind::DeliverPrompt,
                    &fence,
                    19
                )
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert!(store
            .resolve_delegated_inputs("new-lead", "s", &successor, Some(&child.id), &refs, 19)
            .is_ok());
    }
    #[test]
    fn input_shares_survive_restart_and_parent_followup_only_refs_are_not_eligible() {
        let dir = std::env::temp_dir().join(format!("muqun-shares-{}", uuid::Uuid::new_v4()));
        let path = dir.join("work.sqlite");
        let (mut store, root, fence) = fixture_store(
            WorkStore::open(&path).unwrap(),
            DependencyRequirement::ResultAvailable,
            4,
            true,
        );
        let refs = vec![InputRef {
            input_id: root.input_refs[0].input_id.clone(),
            caption: "Child".into(),
            use_: InputUse::ReferenceOnly,
        }];
        let rev = revision(&mut store, &root.id);
        let child = store
            .create_delegated_task_with_inputs(
                "lead",
                "s",
                "child",
                &fence,
                rev,
                input(Some(root.id.clone())),
                vec![],
                refs.clone(),
                10,
            )
            .unwrap()
            .value;
        let fresh = store
            .commit_input_upload(
                "phone",
                "s",
                "later",
                InputUpload {
                    repo_path: "/project".into(),
                    name: "later.txt".into(),
                    mime: "text/plain".into(),
                    size_bytes: 5,
                    sha256: "b".repeat(64),
                },
                11,
            )
            .unwrap();
        let later = vec![InputRef {
            input_id: fresh.input_id.clone(),
            caption: "Later".into(),
            use_: InputUse::ReferenceOnly,
        }];
        let frozen = store
            .resolve_inputs("phone", "s", "/project", Some(&root.id), &later, 12)
            .unwrap();
        let rev = revision(&mut store, &root.id);
        store
            .prepare_delivery_with_inputs(
                "phone",
                "s",
                &root.id,
                &fence.coordinator_attempt_id,
                "root-later",
                rev,
                "root-launch",
                "Followup",
                Some("v1"),
                later.clone(),
                &frozen,
                &"c".repeat(64),
                12,
            )
            .unwrap();
        assert_eq!(
            store
                .resolve_delegated_inputs("lead", "s", &fence, None, &later, 13)
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        drop(store);
        let mut store = WorkStore::open(&path).unwrap();
        assert_eq!(
            store.detail("s", &child.id).unwrap().task.input_refs,
            child.input_refs
        );
        let shares: Vec<InputShare> =
            records(&store.connection, "s", "input_share", &child.id).unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(
            store
                .resolve_inputs(
                    "phone",
                    "s",
                    "/project",
                    Some(&child.id),
                    &refs,
                    49 * 60 * 60 * 1000
                )
                .unwrap(),
            child.input_refs
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT claimed_task FROM work_inputs WHERE id=?1",
                    [&refs[0].input_id],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            root.id
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn delegated_initial_and_followup_version_replay_preserves_exact_fence_and_request() {
        for first in [false, true] {
            for legacy in [false, true] {
                let (mut store, root, fence) = fixture(DependencyRequirement::ResultAvailable, 4);
                let rev = revision(&mut store, &root.id);
                let child = store
                    .create_delegated_task(
                        "lead",
                        "s",
                        "child",
                        &fence,
                        rev,
                        input(Some(root.id)),
                        vec![],
                        10,
                    )
                    .unwrap()
                    .value;
                let mut op = new_operation(
                    &child.id,
                    Some("attempt".into()),
                    OperationKind::DeliverPrompt,
                    11,
                );
                op.delegation_fence = Some(fence.clone());
                op.bootstrap_version = first.then(|| "result-reporting-v1".into());
                let base = inputs::delivery_request_digest(
                    &child.id,
                    "attempt",
                    7,
                    "launch",
                    "text",
                    Some("result-reporting-v1"),
                    &[],
                )
                .unwrap();
                let digest = payload_digest(&(base, &fence)).unwrap();
                put(&store.connection, "s", "operation", &op.id, &child.id, &op).unwrap();
                Request {
                    actor: "lead",
                    session: "s",
                    key: "send",
                    kind: "deliver_prompt",
                    digest: "final-native-envelope".into(),
                }
                .save(&store.connection, &op)
                .unwrap();
                store.connection.execute("INSERT INTO work_delivery_inputs(operation_id,request_digest,final_prompt_digest,instruction_version) VALUES(?1,?2,'final',?3)",params![op.id,digest,if legacy {None}else{Some(encoded(&Some("result-reporting-v1")).unwrap())}]).unwrap();
                assert_eq!(
                    store
                        .replay_delegated_delivery(
                            "lead",
                            "s",
                            &child.id,
                            "attempt",
                            "send",
                            7,
                            "launch",
                            "text",
                            Some("managed-delegation-v2"),
                            &[],
                            &fence
                        )
                        .unwrap()
                        .unwrap()
                        .value
                        .id,
                    op.id
                );
                for (text, launch, rev) in [
                    ("changed", "launch", 7),
                    ("text", "other", 7),
                    ("text", "launch", 8),
                ] {
                    assert_eq!(
                        store
                            .replay_delegated_delivery(
                                "lead",
                                "s",
                                &child.id,
                                "attempt",
                                "send",
                                rev,
                                launch,
                                text,
                                Some("managed-delegation-v2"),
                                &[],
                                &fence
                            )
                            .unwrap_err()
                            .0,
                        FailureCode::RequestKeyConflict
                    );
                }
                let mut stale = fence.clone();
                stale.coordinator_epoch += 1;
                assert!(store
                    .replay_delegated_delivery(
                        "lead",
                        "s",
                        &child.id,
                        "attempt",
                        "send",
                        7,
                        "launch",
                        "text",
                        Some("managed-delegation-v2"),
                        &[],
                        &stale
                    )
                    .is_err());
            }
        }
    }
    #[test]
    fn delegated_dependency_update_receipts_are_exact_and_authority_is_not_revived() {
        let (mut store, root, fence) = fixture(DependencyRequirement::HumanAccepted, 4);
        let prerequisite = child(&mut store, &root, &fence, "prerequisite", vec![]);
        let dependent = child(&mut store, &root, &fence, "dependent", vec![]);
        let rev = revision(&mut store, &dependent.id);
        let deps = vec![TaskDependency {
            prerequisite_task_id: prerequisite.id,
            submission_id: None,
        }];
        let updated = store
            .set_delegated_dependencies(
                "lead",
                "s",
                &dependent.id,
                "bind",
                rev,
                deps.clone(),
                &fence,
                10,
            )
            .unwrap();
        assert!(!updated.replayed);
        assert_eq!(
            store.dependency_readiness("s", &dependent.id).unwrap(),
            DependencyReadiness::WaitingForResult
        );
        assert!(
            store
                .replay_delegated_dependencies(
                    "lead",
                    "s",
                    &dependent.id,
                    "bind",
                    rev,
                    &deps,
                    &fence
                )
                .unwrap()
                .unwrap()
                .replayed
        );
        assert!(
            store
                .set_delegated_dependencies(
                    "lead",
                    "s",
                    &dependent.id,
                    "bind",
                    rev,
                    deps.clone(),
                    &fence,
                    11
                )
                .unwrap()
                .replayed
        );
        let receipt = store
            .get_delegated_receipt("lead", "s", &fence, OperationKind::SetDependencies, "bind")
            .unwrap();
        assert_eq!(
            serde_json::from_value::<Task>(receipt.value).unwrap().id,
            dependent.id
        );
        assert_eq!(
            store
                .replay_delegated_dependencies("lead", "s", &dependent.id, "bind", rev, &[], &fence)
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
        assert_eq!(
            store
                .replay_delegated_dependencies(
                    "lead",
                    "s",
                    &dependent.id,
                    "bind",
                    rev + 1,
                    &deps,
                    &fence
                )
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
        assert!(store
            .get_delegated_receipt("other", "s", &fence, OperationKind::SetDependencies, "bind")
            .is_err());
        assert_eq!(
            store
                .set_delegated_dependencies(
                    "lead",
                    "s",
                    &dependent.id,
                    "new",
                    rev,
                    deps.clone(),
                    &fence,
                    12
                )
                .unwrap_err()
                .0,
            FailureCode::RevisionConflict
        );
        let mut root: Task = load(&store.connection, "s", "task", &root.id).unwrap();
        root.delegation.coordinator_epoch += 1;
        put(&store.connection, "s", "task", &root.id, &root.id, &root).unwrap();
        assert!(store
            .set_delegated_dependencies(
                "lead",
                "s",
                &dependent.id,
                "bind",
                rev,
                deps.clone(),
                &fence,
                13
            )
            .is_err());
        let mut successor = fence;
        successor.coordinator_epoch += 1;
        assert!(store
            .get_delegated_receipt(
                "lead",
                "s",
                &successor,
                OperationKind::SetDependencies,
                "bind"
            )
            .is_err());
        assert!(store
            .replay_delegated_dependencies(
                "lead",
                "s",
                &dependent.id,
                "bind",
                rev,
                &deps,
                &successor
            )
            .is_err());
    }
}
