use super::*;

fn request<'a>(
    actor: &'a str,
    session: &'a str,
    task: &str,
    attempt: &str,
    input: &'a InterruptInput,
) -> WorkResult<Request<'a>> {
    if !native_identity(&input.expected_instance_id)
        || !native_identity(&input.expected_native_owner_epoch)
    {
        return Err(invalid());
    }
    Ok(Request {
        actor,
        session,
        key: &input.request_key,
        kind: "interrupt_attempt",
        digest: payload_digest(&(task, attempt, input))?,
    })
}
pub(super) fn check_binding(db: &Connection, session: &str, op: &Operation) -> WorkResult<()> {
    let attempt: Attempt = load(
        db,
        session,
        "attempt",
        op.attempt_id.as_deref().ok_or_else(invalid)?,
    )?;
    if attempt.task_id != op.task_id {
        return Err(WorkError(FailureCode::ScopeMismatch));
    }
    if !reservation_active(&attempt) {
        return Err(WorkError(FailureCode::NotReady));
    }
    if op.resources.instance_id.is_none()
        || op.interruption_owner_epoch.is_none()
        || attempt.binding.instance_id != op.resources.instance_id
        || attempt.lifecycle.native_owner_epoch != op.interruption_owner_epoch
    {
        return Err(WorkError(FailureCode::InstanceChanged));
    }
    match_native_binding(
        &attempt,
        op.resources.instance_id.as_deref().ok_or_else(invalid)?,
        op.interruption_owner_epoch.as_deref().ok_or_else(invalid)?,
    )
}
impl WorkStore {
    pub fn replay_interrupt(
        &self,
        actor: &str,
        session: &str,
        task: &str,
        attempt: &str,
        input: &InterruptInput,
    ) -> WorkResult<Option<Mutation<Operation>>> {
        request(actor, session, task, attempt, input)?
            .replay::<Operation>(&self.connection)?
            .map(|old| {
                Ok(Mutation {
                    value: load(&self.connection, session, "operation", &old.id)?,
                    replayed: true,
                })
            })
            .transpose()
    }
    /// Paired authority must guard this transaction. No local reporting/control grant can admit interruption.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_interrupt(
        &mut self,
        actor: &str,
        session: &str,
        task_id: &str,
        attempt_id: &str,
        input: InterruptInput,
        now: i64,
    ) -> WorkResult<Mutation<Operation>> {
        let request = request(actor, session, task_id, attempt_id, &input)?;
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
        let mut op = new_operation(
            task_id,
            Some(attempt_id.into()),
            OperationKind::InterruptAttempt,
            now,
        );
        op.resources = attempt.binding.clone();
        op.interruption_owner_epoch = Some(input.expected_native_owner_epoch.clone());
        if op.resources.instance_id.as_deref() != Some(input.expected_instance_id.as_str()) {
            return Err(WorkError(FailureCode::InstanceChanged));
        }
        check_binding(&tx, session, &op)?;
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM records WHERE session=?1 AND kind='operation' AND task_id=?2 AND json_extract(body,'$.attempt_id')=?3 AND json_extract(body,'$.kind')='interrupt_attempt' AND json_extract(body,'$.state') IN ('prepared','submitting','unconfirmed'))", params![session,task_id,attempt_id], |row|row.get(0))?;
        if pending {
            return Err(WorkError(FailureCode::NotReady));
        }
        put(&tx, session, "operation", &op.id, task_id, &op)?;
        reserve_fact(&tx, session, &op, "reserved_interruption")?;
        // Existing public operation event avoids a new change-stream discriminator.
        changed(&tx, &mut task, "operation_changed", &op.id, now)?;
        request.save(&tx, &op)?;
        tx.commit()?;
        Ok(Mutation {
            value: op,
            replayed: false,
        })
    }
    /// Persist a terminal fact without rechecking current authority or lifecycle after native I/O.
    /// Invalid success evidence is retained as uncertainty, never as proof of a write or exit.
    pub fn finalize_interrupt(
        &mut self,
        session: &str,
        operation_id: &str,
        outcome: InterruptionOutcome,
        now: i64,
    ) -> WorkResult<Operation> {
        let tx = self.connection.transaction()?;
        let mut op: Operation = load(&tx, session, "operation", operation_id)?;
        if op.kind != OperationKind::InterruptAttempt || op.state != OperationState::Submitting {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        let (state, code, receipt) = match outcome {
            InterruptionOutcome::Acknowledged(receipt) if valid_receipt(&op, &receipt) => {
                (OperationState::Acknowledged, None, Some(receipt))
            }
            InterruptionOutcome::Refused(code)
                if matches!(
                    code,
                    FailureCode::ResourceLimit
                        | FailureCode::InstanceChanged
                        | FailureCode::CapabilityUnavailable
                ) =>
            {
                (OperationState::Refused, Some(code), None)
            }
            _ => (
                OperationState::Unconfirmed,
                Some(FailureCode::DeliveryUnconfirmed),
                None,
            ),
        };
        op.state = state;
        op.failure_code = code;
        op.interruption_receipt = receipt;
        op.updated_at_ms = now;
        fulfill_fact(
            &tx,
            session,
            &op,
            "reserved_interruption",
            "interruption_fact",
            &serde_json::json!({"operation_id":op.id,"state":op.state,"failure_code":op.failure_code,"receipt":op.interruption_receipt}),
        )?;
        save_operation(&tx, session, &op, now)?;
        tx.commit()?;
        Ok(op)
    }
}
fn valid_receipt(op: &Operation, receipt: &InterruptionReceipt) -> bool {
    receipt.operation_id == op.id
        && Some(receipt.launch_id.as_str()) == op.resources.instance_id.as_deref()
        && Some(receipt.owner_epoch.as_str()) == op.interruption_owner_epoch.as_deref()
        && native_identity(&receipt.receipt_id)
        && receipt.key == "Escape"
        && receipt.bytes_written == 1
        && receipt.input_disposition == "written"
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(store: &mut WorkStore) -> (Task, Attempt) {
        let task = store
            .create_task(
                "device",
                "s",
                "task",
                CreateTask {
                    repo_path: "/repo".into(),
                    title: "Task".into(),
                    brief: "Brief".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 0,
                    },
                },
                1,
            )
            .unwrap()
            .value;
        let op = store
            .prepare_attempt(
                "device",
                "s",
                &task.id,
                "start",
                task.revision,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "digest",
                2,
            )
            .unwrap()
            .value;
        let mut attempt: Attempt = load(
            &store.connection,
            "s",
            "attempt",
            op.attempt_id.as_deref().unwrap(),
        )
        .unwrap();
        attempt.binding.instance_id = Some("launch".into());
        attempt.lifecycle.native_owner_epoch = Some("owner".into());
        attempt.lifecycle.launch_phase = LaunchPhase::LaunchConfirmed;
        put(
            &store.connection,
            "s",
            "attempt",
            &attempt.id,
            &task.id,
            &attempt,
        )
        .unwrap();
        let mut start = op;
        start.state = OperationState::Acknowledged;
        put(
            &store.connection,
            "s",
            "operation",
            &start.id,
            &task.id,
            &start,
        )
        .unwrap();
        (task, attempt)
    }
    fn input(store: &WorkStore, task: &Task, key: &str) -> InterruptInput {
        InterruptInput {
            request_key: key.into(),
            expected_revision: load::<Task>(&store.connection, "s", "task", &task.id)
                .unwrap()
                .revision,
            expected_instance_id: "launch".into(),
            expected_native_owner_epoch: "owner".into(),
        }
    }
    fn receipt(op: &Operation) -> InterruptionReceipt {
        InterruptionReceipt {
            operation_id: op.id.clone(),
            launch_id: "launch".into(),
            owner_epoch: "owner".into(),
            receipt_id: "receipt".into(),
            key: "Escape".into(),
            bytes_written: 1,
            input_disposition: "written".into(),
        }
    }
    #[test]
    fn interruption_coexists_with_uncertain_prompt_but_has_cross_device_single_flight() {
        let mut store = WorkStore::in_memory().unwrap();
        let (task, attempt) = fixture(&mut store);
        let mut prompt = new_operation(
            &task.id,
            Some(attempt.id.clone()),
            OperationKind::DeliverPrompt,
            3,
        );
        prompt.state = OperationState::Unconfirmed;
        put(
            &store.connection,
            "s",
            "operation",
            &prompt.id,
            &task.id,
            &prompt,
        )
        .unwrap();
        let request = input(&store, &task, "interrupt");
        let op = store
            .prepare_interrupt("device", "s", &task.id, &attempt.id, request.clone(), 4)
            .unwrap()
            .value;
        let second = input(&store, &task, "second");
        assert_eq!(
            store
                .prepare_interrupt("other", "s", &task.id, &attempt.id, second, 5)
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
        store.begin_operation("s", &op.id, 6).unwrap();
        assert!(store
            .finalize_operation(
                "s",
                &op.id,
                OperationOutcome {
                    state: OperationState::Acknowledged,
                    resources: NativeBinding::default(),
                    failure_code: None
                },
                7
            )
            .is_err());
        let done = store
            .finalize_interrupt(
                "s",
                &op.id,
                InterruptionOutcome::Acknowledged(receipt(&op)),
                8,
            )
            .unwrap();
        assert_eq!(done.state, OperationState::Acknowledged);
        let replay = store
            .replay_interrupt("device", "s", &task.id, &attempt.id, &request)
            .unwrap()
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.value.interruption_receipt, done.interruption_receipt);
        let mut changed = request;
        changed.expected_native_owner_epoch = "replacement".into();
        assert_eq!(
            store
                .replay_interrupt("device", "s", &task.id, &attempt.id, &changed)
                .unwrap_err()
                .0,
            FailureCode::RequestKeyConflict
        );
        assert_eq!(
            store.get_operation("s", &prompt.id).unwrap().state,
            OperationState::Unconfirmed
        );
        let detail = store.detail("s", &task.id).unwrap();
        assert!(reservation_active(&detail.attempts[0]));
        assert!(detail.results.is_empty());
        let next = input(&store, &task, "next");
        assert!(store
            .prepare_interrupt("other", "s", &task.id, &attempt.id, next, 9)
            .is_ok());
    }
    #[test]
    fn interruption_rejects_wrong_binding_and_preserves_uncertainty_across_restart() {
        let dir = std::env::temp_dir().join(format!("interrupt-{}", id()));
        let path = dir.join("work.sqlite");
        let mut store = WorkStore::open(&path).unwrap();
        let (task, attempt) = fixture(&mut store);
        let mut wrong = input(&store, &task, "bad");
        wrong.expected_native_owner_epoch = "old".into();
        assert_eq!(
            store
                .prepare_interrupt("device", "s", &task.id, &attempt.id, wrong, 3)
                .unwrap_err()
                .0,
            FailureCode::InstanceChanged
        );
        let request = input(&store, &task, "valid");
        let op = store
            .prepare_interrupt("device", "s", &task.id, &attempt.id, request.clone(), 4)
            .unwrap()
            .value;
        store.begin_operation("s", &op.id, 5).unwrap();
        drop(store);
        let mut store = WorkStore::open(&path).unwrap();
        assert_eq!(
            store.get_operation("s", &op.id).unwrap().state,
            OperationState::Unconfirmed
        );
        assert!(
            store
                .replay_interrupt("device", "s", &task.id, &attempt.id, &request)
                .unwrap()
                .unwrap()
                .replayed
        );
        let next = input(&store, &task, "next");
        assert_eq!(
            store
                .prepare_interrupt("other", "s", &task.id, &attempt.id, next, 6)
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
        assert_eq!(
            store
                .get_receipt("other", "s", OperationKind::InterruptAttempt, "valid")
                .unwrap_err()
                .0,
            FailureCode::NotFound
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn interruption_terminal_receipt_uses_reserved_capacity_and_malformed_ack_is_unknown() {
        for malformed in [false, true] {
            let mut store = WorkStore::in_memory().unwrap();
            let (task, attempt) = fixture(&mut store);
            let request = input(&store, &task, "interrupt");
            let op = store
                .prepare_interrupt("device", "s", &task.id, &attempt.id, request, 4)
                .unwrap()
                .value;
            store.begin_operation("s", &op.id, 5).unwrap();
            let count: u64 = store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM records WHERE task_id=?1",
                    [&task.id],
                    |r| r.get(0),
                )
                .unwrap();
            for _ in count..1024 {
                put(
                    &store.connection,
                    "s",
                    "padding",
                    &id(),
                    &task.id,
                    &serde_json::json!({}),
                )
                .unwrap();
            }
            let mut proof = receipt(&op);
            if malformed {
                proof.bytes_written = 2;
            }
            let done = store
                .finalize_interrupt("s", &op.id, InterruptionOutcome::Acknowledged(proof), 6)
                .unwrap();
            assert_eq!(
                done.state,
                if malformed {
                    OperationState::Unconfirmed
                } else {
                    OperationState::Acknowledged
                }
            );
            assert_eq!(done.interruption_receipt.is_some(), !malformed);
            assert!(reservation_active(
                &store.detail("s", &task.id).unwrap().attempts[0]
            ));
            assert_eq!(
                put(
                    &store.connection,
                    "s",
                    "padding",
                    &id(),
                    &task.id,
                    &serde_json::json!({})
                )
                .unwrap_err()
                .0,
                FailureCode::ResourceLimit
            );
        }
    }
    #[test]
    fn interruption_admission_is_scoped_quota_atomic_and_blocks_new_prompt() {
        let mut store = WorkStore::in_memory().unwrap();
        let (task, attempt) = fixture(&mut store);
        let request = input(&store, &task, "key");
        assert!(store
            .prepare_interrupt(
                "device",
                "foreign",
                &task.id,
                &attempt.id,
                request.clone(),
                3
            )
            .is_err());
        let mut stale = request.clone();
        stale.expected_revision = 0;
        assert_eq!(
            store
                .prepare_interrupt("device", "s", &task.id, &attempt.id, stale, 3)
                .unwrap_err()
                .0,
            FailureCode::RevisionConflict
        );
        // Leave only one row: an operation plus terminal fact must be rejected atomically.
        let count: u64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM records WHERE task_id=?1",
                [&task.id],
                |r| r.get(0),
            )
            .unwrap();
        for _ in count..1023 {
            put(
                &store.connection,
                "s",
                "padding",
                &id(),
                &task.id,
                &serde_json::json!({}),
            )
            .unwrap();
        }
        assert_eq!(
            store
                .prepare_interrupt("device", "s", &task.id, &attempt.id, request.clone(), 3)
                .unwrap_err()
                .0,
            FailureCode::ResourceLimit
        );
        assert!(store
            .replay_interrupt("device", "s", &task.id, &attempt.id, &request)
            .unwrap()
            .is_none());
        store
            .connection
            .execute("DELETE FROM records WHERE kind='padding'", [])
            .unwrap();
        let op = store
            .prepare_interrupt("device", "s", &task.id, &attempt.id, request, 4)
            .unwrap()
            .value;
        let revision = input(&store, &task, "unused").expected_revision;
        assert_eq!(
            store
                .prepare_delivery(
                    "device",
                    "s",
                    &task.id,
                    &attempt.id,
                    "prompt",
                    revision,
                    "launch",
                    "hello",
                    5
                )
                .unwrap_err()
                .0,
            FailureCode::NotReady
        );
        // A lifecycle change between preparation and dispatch prevents the effect.
        let mut replaced = attempt;
        replaced.lifecycle.native_owner_epoch = Some("replacement".into());
        put(
            &store.connection,
            "s",
            "attempt",
            &replaced.id,
            &task.id,
            &replaced,
        )
        .unwrap();
        assert_eq!(
            store.begin_operation("s", &op.id, 6).unwrap_err().0,
            FailureCode::InstanceChanged
        );
        assert_eq!(
            store.get_operation("s", &op.id).unwrap().state,
            OperationState::Prepared
        );
    }
}
