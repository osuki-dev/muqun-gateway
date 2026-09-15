use super::{model::*, store::WorkStore};
fn reconciliation_intent(
    store: &mut WorkStore,
    task: &Task,
    attempt_id: &str,
    key: &str,
) -> (Operation, ReconcileInput) {
    let detail = store.detail("session", &task.id).unwrap();
    let attempt = detail.attempts.iter().find(|a| a.id == attempt_id).unwrap();
    let input = ReconcileInput {
        request_key: key.into(),
        expected_revision: detail.task.revision,
        expected_instance_id: attempt.binding.instance_id.clone(),
        expected_native_owner_epoch: attempt.lifecycle.native_owner_epoch.clone(),
    };
    let op = store
        .prepare_reconciliation("actor", "session", &task.id, attempt_id, input.clone(), 20)
        .unwrap()
        .value;
    store.begin_operation("session", &op.id, 21).unwrap();
    (op, input)
}
fn finish_check(
    store: &mut WorkStore,
    task: &Task,
    op: &Operation,
    proof: ReconciliationEvidence,
) -> WorkResult<ReconciliationReceipt> {
    let revision = store.detail("session", &task.id)?.task.revision;
    store.finish_reconciliation("session", &op.id, revision, proof, 22)
}
#[test]
fn lifecycle_release_and_dispatch_fence_have_only_one_winner() {
    for dispatch_wins in [false, true] {
        let mut store = WorkStore::in_memory().unwrap();
        let task = store
            .create_task("actor", "session", "task", input(), 1)
            .unwrap()
            .value;
        let start = reserve_lead(&mut store, &task, "start").unwrap().value;
        store.begin_operation("session", &start.id, 3).unwrap();
        store
            .checkpoint_operation(
                "session",
                &start.id,
                NativeBinding {
                    workspace_id: Some("workspace".into()),
                    ..Default::default()
                },
                4,
            )
            .unwrap();
        let (check, request) = reconciliation_intent(
            &mut store,
            &task,
            start.attempt_id.as_deref().unwrap(),
            "check",
        );
        if dispatch_wins {
            store.claim_start_dispatch("session", &start.id, 5).unwrap();
            assert_eq!(
                finish_check(
                    &mut store,
                    &task,
                    &check,
                    ReconciliationEvidence::NotDispatched
                )
                .unwrap_err()
                .0,
                FailureCode::NotReady
            );
            let unknown =
                finish_check(&mut store, &task, &check, ReconciliationEvidence::Unknown).unwrap();
            assert_eq!(unknown.reservation, Reservation::Reserved);
            assert_eq!(
                reserve_lead(&mut store, &task, "unsafe").unwrap_err().0,
                FailureCode::ResourceLimit
            );
        } else {
            let released = finish_check(
                &mut store,
                &task,
                &check,
                ReconciliationEvidence::NotDispatched,
            )
            .unwrap();
            assert_eq!(released.reservation, Reservation::Released);
            let late = store
                .checkpoint_operation(
                    "session",
                    &start.id,
                    NativeBinding {
                        pane_id: Some("late-pane".into()),
                        ..Default::default()
                    },
                    25,
                )
                .unwrap();
            assert_eq!(late.state, OperationState::Refused);
            assert_eq!(
                store
                    .checkpoint_operation(
                        "session",
                        &start.id,
                        NativeBinding {
                            instance_id: Some("forbidden-launch".into()),
                            ..Default::default()
                        },
                        26
                    )
                    .unwrap_err()
                    .0,
                FailureCode::RevisionConflict
            );
            assert_eq!(
                store
                    .claim_start_dispatch("session", &start.id, 30)
                    .unwrap_err()
                    .0,
                FailureCode::RevisionConflict
            );
            let detail = store.detail("session", &task.id).unwrap();
            assert_eq!(
                detail.attempts[0].binding.workspace_id.as_deref(),
                Some("workspace")
            );
            assert!(reserve_lead(&mut store, &task, "replacement").is_ok());
            let replay = store
                .prepare_reconciliation(
                    "actor",
                    "session",
                    &task.id,
                    start.attempt_id.as_deref().unwrap(),
                    request.clone(),
                    31,
                )
                .unwrap();
            assert!(replay.replayed);
            let receipt = store
                .get_receipt("actor", "session", OperationKind::ReconcileAttempt, "check")
                .unwrap();
            assert_eq!(receipt.value["receipt_type"], "reconciliation");
            assert_eq!(
                receipt.value["receipt"]["task_revision"],
                released.task_revision
            );
            assert_eq!(
                store
                    .get_receipt("other", "session", OperationKind::ReconcileAttempt, "check")
                    .unwrap_err()
                    .0,
                FailureCode::NotFound
            );
            let mut changed = request;
            changed.expected_revision += 1;
            assert_eq!(
                store
                    .prepare_reconciliation(
                        "actor",
                        "session",
                        &task.id,
                        start.attempt_id.as_deref().unwrap(),
                        changed,
                        32
                    )
                    .unwrap_err()
                    .0,
                FailureCode::RequestKeyConflict
            );
        }
    }
}
#[test]
fn lifecycle_no_process_refusal_requires_persisted_matching_receipt() {
    let mut store = WorkStore::in_memory().unwrap();
    let task = store
        .create_task("actor", "session", "task", input(), 1)
        .unwrap()
        .value;
    let start = reserve_lead(&mut store, &task, "start").unwrap().value;
    store.begin_operation("session", &start.id, 3).unwrap();
    store.claim_start_dispatch("session", &start.id, 4).unwrap();
    let (check, _) = reconciliation_intent(
        &mut store,
        &task,
        start.attempt_id.as_deref().unwrap(),
        "check",
    );
    let proof = ReconciliationEvidence::NativeNotStarted {
        start_operation_id: start.id.clone(),
        owner_epoch: "epoch".into(),
        receipt_id: "receipt".into(),
    };
    assert_eq!(
        finish_check(&mut store, &task, &check, proof.clone())
            .unwrap_err()
            .0,
        FailureCode::NotReady
    );
    store
        .record_start_refusal("session", &start.id, "epoch", "receipt", 25)
        .unwrap();
    assert_eq!(
        store
            .record_start_refusal("session", &start.id, "epoch", "other", 26)
            .unwrap_err()
            .0,
        FailureCode::InstanceChanged
    );
    assert_eq!(
        store
            .confirm_start_launch(
                "session",
                &start.id,
                NativeBinding {
                    instance_id: Some("launch".into()),
                    ..Default::default()
                },
                "epoch",
                27
            )
            .unwrap_err()
            .0,
        FailureCode::RevisionConflict
    );
    assert_eq!(
        finish_check(&mut store, &task, &check, proof)
            .unwrap()
            .reservation,
        Reservation::Released
    );
}
#[test]
fn lifecycle_exit_releases_exact_descendant_budget_and_fences_prepared_input() {
    let mut store = WorkStore::in_memory().unwrap();
    let mut root_input = input();
    root_input.policy.max_workers = 1;
    let root = store
        .create_task("actor", "session", "root", root_input, 1)
        .unwrap()
        .value;
    let child = create_child(&mut store, &root, "child");
    let sibling = create_child(&mut store, &root, "sibling");
    let start = reserve_lead(&mut store, &child, "start").unwrap().value;
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
    let aid = start.attempt_id.as_deref().unwrap();
    let rev = store.detail("session", &child.id).unwrap().task.revision;
    let delivery = store
        .prepare_delivery(
            "actor", "session", &child.id, aid, "delivery", rev, "launch", "prompt", 7,
        )
        .unwrap()
        .value;
    let (check, _) = reconciliation_intent(&mut store, &child, aid, "live");
    assert_eq!(
        finish_check(
            &mut store,
            &child,
            &check,
            ReconciliationEvidence::Live {
                instance_id: "launch".into(),
                owner_epoch: "epoch".into()
            }
        )
        .unwrap()
        .reservation,
        Reservation::Reserved
    );
    assert_eq!(
        reserve_lead(&mut store, &sibling, "blocked").unwrap_err().0,
        FailureCode::ResourceLimit
    );
    let (check, _) = reconciliation_intent(&mut store, &child, aid, "exit");
    assert_eq!(
        finish_check(
            &mut store,
            &child,
            &check,
            ReconciliationEvidence::Exited {
                instance_id: "launch".into(),
                owner_epoch: "other-epoch".into(),
                receipt_id: "exit".into()
            }
        )
        .unwrap_err()
        .0,
        FailureCode::InstanceChanged
    );
    assert_eq!(
        finish_check(
            &mut store,
            &child,
            &check,
            ReconciliationEvidence::Exited {
                instance_id: "launch".into(),
                owner_epoch: "epoch".into(),
                receipt_id: "exit".into()
            }
        )
        .unwrap()
        .reservation,
        Reservation::Released
    );
    assert_eq!(
        store
            .begin_operation("session", &delivery.id, 40)
            .unwrap_err()
            .0,
        FailureCode::NotReady
    );
    assert!(reserve_lead(&mut store, &sibling, "replacement").is_ok());
    let snapshot = store.detail("session", &child.id).unwrap();
    assert_eq!(
        store
            .finish_reconciliation("session", &check.id, 0, ReconciliationEvidence::Unknown, 99)
            .unwrap()
            .observation,
        LifecycleObservation::Exited
    );
    assert_eq!(
        snapshot.cursor,
        store.detail("session", &child.id).unwrap().cursor
    );
}
#[test]
fn lifecycle_restart_keeps_claimed_and_legacy_attempts_reserved() {
    let dir = std::env::temp_dir().join(format!("muqun-lifecycle-{}", uuid::Uuid::new_v4()));
    let path = dir.join("work.sqlite");
    let (task, start) = {
        let mut store = WorkStore::open(&path).unwrap();
        let task = store
            .create_task("actor", "session", "task", input(), 1)
            .unwrap()
            .value;
        let start = reserve_lead(&mut store, &task, "start").unwrap().value;
        store.begin_operation("session", &start.id, 3).unwrap();
        store.claim_start_dispatch("session", &start.id, 4).unwrap();
        (task, start)
    };
    let mut store = WorkStore::open(&path).unwrap();
    assert_eq!(
        store.get_operation("session", &start.id).unwrap().state,
        OperationState::Unconfirmed
    );
    let (check, _) = reconciliation_intent(
        &mut store,
        &task,
        start.attempt_id.as_deref().unwrap(),
        "check",
    );
    assert_eq!(
        finish_check(
            &mut store,
            &task,
            &check,
            ReconciliationEvidence::NotDispatched
        )
        .unwrap_err()
        .0,
        FailureCode::NotReady
    );
    finish_check(&mut store, &task, &check, ReconciliationEvidence::Unknown).unwrap();
    assert_eq!(
        reserve_lead(&mut store, &task, "unsafe").unwrap_err().0,
        FailureCode::ResourceLimit
    );
    drop(store);
    // Simulate a pre-lifecycle persisted JSON row without rewriting any operation evidence.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE records SET body=json_remove(body,'$.lifecycle') WHERE kind='attempt'",
        [],
    )
    .unwrap();
    drop(db);
    let mut store = WorkStore::open(&path).unwrap();
    let detail = store.detail("session", &task.id).unwrap();
    assert_eq!(
        detail.attempts[0].lifecycle.launch_phase,
        LaunchPhase::LegacyUnknown
    );
    assert_eq!(
        detail.attempts[0].lifecycle.reservation,
        Reservation::Reserved
    );
    assert_eq!(
        reserve_lead(&mut store, &task, "legacy-unsafe")
            .unwrap_err()
            .0,
        FailureCode::ResourceLimit
    );
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn lifecycle_concurrent_dispatch_and_release_are_serialized() {
    use std::sync::{Arc, Barrier, Mutex};
    let mut store = WorkStore::in_memory().unwrap();
    let task = store
        .create_task("actor", "session", "task", input(), 1)
        .unwrap()
        .value;
    let start = reserve_lead(&mut store, &task, "start").unwrap().value;
    store.begin_operation("session", &start.id, 3).unwrap();
    let (check, _) = reconciliation_intent(
        &mut store,
        &task,
        start.attempt_id.as_deref().unwrap(),
        "check",
    );
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    let store = Arc::new(Mutex::new(store));
    let barrier = Arc::new(Barrier::new(2));
    let (dispatch, release) = std::thread::scope(|scope| {
        let dispatch = scope.spawn(|| {
            barrier.wait();
            store
                .lock()
                .unwrap()
                .claim_start_dispatch("session", &start.id, 30)
        });
        let release = scope.spawn(|| {
            barrier.wait();
            store.lock().unwrap().finish_reconciliation(
                "session",
                &check.id,
                revision,
                ReconciliationEvidence::NotDispatched,
                31,
            )
        });
        (dispatch.join().unwrap(), release.join().unwrap())
    });
    assert_ne!(dispatch.is_ok(), release.is_ok());
    let attempt = store
        .lock()
        .unwrap()
        .detail("session", &task.id)
        .unwrap()
        .attempts
        .remove(0);
    assert_eq!(
        attempt.lifecycle.reservation == Reservation::Released,
        release.is_ok()
    );
}
#[test]
fn lifecycle_reconciliation_requires_explicit_nullable_identity_fields() {
    let valid = serde_json::json!({"request_key":"key","expected_revision":1,"expected_instance_id":null,"expected_native_owner_epoch":null});
    assert!(serde_json::from_value::<ReconcileInput>(valid.clone()).is_ok());
    for field in ["expected_instance_id", "expected_native_owner_epoch"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ReconcileInput>(missing).is_err());
    }
}
#[test]
fn lifecycle_revision_race_does_not_block_input_but_unknown_delivery_does() {
    let mut store = WorkStore::in_memory().unwrap();
    let task = store
        .create_task("actor", "session", "task", input(), 1)
        .unwrap()
        .value;
    let start = reserve_lead(&mut store, &task, "start").unwrap().value;
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
    let aid = start.attempt_id.as_deref().unwrap();
    let (check, _) = reconciliation_intent(&mut store, &task, aid, "check");
    let revision_before_native_reply = store.detail("session", &task.id).unwrap().task.revision;
    // A result arrives while the read-only native lifecycle query is pending.
    store
        .submit_result(
            "actor",
            "session",
            &task.id,
            "result",
            revision_before_native_reply,
            ResultInput {
                attempt_id: aid.into(),
                summary: "Result arrived during check".into(),
                artifacts: vec![],
                evidence: vec![],
            },
            30,
        )
        .unwrap();
    assert_eq!(
        store
            .finish_reconciliation(
                "session",
                &check.id,
                revision_before_native_reply,
                ReconciliationEvidence::Live {
                    instance_id: "launch".into(),
                    owner_epoch: "epoch".into()
                },
                31
            )
            .unwrap_err()
            .0,
        FailureCode::RevisionConflict
    );
    assert_eq!(
        store.get_operation("session", &check.id).unwrap().state,
        OperationState::Submitting
    );
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    let delivery = store
        .prepare_delivery(
            "actor",
            "session",
            &task.id,
            aid,
            "followup",
            revision,
            "launch",
            "Follow up",
            32,
        )
        .unwrap()
        .value;
    store
        .refuse_prepared_operation("session", &delivery.id, FailureCode::NotReady, 33)
        .unwrap();
    // Recovery of an interrupted read-only check also does not represent uncertain input.
    store
        .finalize_operation(
            "session",
            &check.id,
            OperationOutcome {
                state: OperationState::Unconfirmed,
                resources: Default::default(),
                failure_code: Some(FailureCode::DeliveryUnconfirmed),
            },
            34,
        )
        .unwrap();
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    let delivery = store
        .prepare_delivery(
            "actor",
            "session",
            &task.id,
            aid,
            "second-followup",
            revision,
            "launch",
            "Follow up explicitly",
            35,
        )
        .unwrap()
        .value;
    store.begin_operation("session", &delivery.id, 36).unwrap();
    store
        .finalize_operation(
            "session",
            &delivery.id,
            OperationOutcome {
                state: OperationState::Unconfirmed,
                resources: Default::default(),
                failure_code: Some(FailureCode::DeliveryUnconfirmed),
            },
            37,
        )
        .unwrap();
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    assert_eq!(
        store
            .prepare_delivery(
                "actor",
                "session",
                &task.id,
                aid,
                "unsafe-third",
                revision,
                "launch",
                "Must stay blocked",
                38
            )
            .unwrap_err()
            .0,
        FailureCode::NotReady
    );
}
#[test]
fn lifecycle_release_receipt_and_resource_history_survive_restart() {
    let dir = std::env::temp_dir().join(format!("muqun-release-{}", uuid::Uuid::new_v4()));
    let path = dir.join("work.sqlite");
    let (task, check_id) = {
        let mut store = WorkStore::open(&path).unwrap();
        let task = store
            .create_task("actor", "session", "task", input(), 1)
            .unwrap()
            .value;
        let start = reserve_lead(&mut store, &task, "start").unwrap().value;
        store.begin_operation("session", &start.id, 3).unwrap();
        store
            .checkpoint_operation(
                "session",
                &start.id,
                NativeBinding {
                    workspace_id: Some("workspace".into()),
                    ..Default::default()
                },
                4,
            )
            .unwrap();
        let (check, _) = reconciliation_intent(
            &mut store,
            &task,
            start.attempt_id.as_deref().unwrap(),
            "check",
        );
        finish_check(
            &mut store,
            &task,
            &check,
            ReconciliationEvidence::NotDispatched,
        )
        .unwrap();
        (task, check.id)
    };
    let mut store = WorkStore::open(&path).unwrap();
    assert_eq!(
        store
            .get_reconciliation("session", &check_id)
            .unwrap()
            .reservation,
        Reservation::Released
    );
    let history = store.detail("session", &task.id).unwrap();
    assert_eq!(
        history.attempts[0].binding.workspace_id.as_deref(),
        Some("workspace")
    );
    assert_eq!(
        history.attempts[0].lifecycle.reservation,
        Reservation::Released
    );
    assert!(reserve_lead(&mut store, &task, "replacement").is_ok());
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn lifecycle_terminal_facts_use_reserved_quota_under_concurrent_result_growth() {
    for initial_count in [1020, 1022] {
        let dir = std::env::temp_dir().join(format!("muqun-quota-{}", uuid::Uuid::new_v4()));
        let path = dir.join("work.sqlite");
        let mut store = WorkStore::open(&path).unwrap();
        let task = store
            .create_task("actor", "session", "task", input(), 1)
            .unwrap()
            .value;
        let start = reserve_lead(&mut store, &task, "start").unwrap().value;
        store.begin_operation("session", &start.id, 3).unwrap();
        store.claim_start_dispatch("session", &start.id, 4).unwrap();
        let aid = start.attempt_id.as_deref().unwrap();
        // Seed valid historical rows efficiently. The admission/finalization behavior
        // below uses the real public store APIs, including the final competing result.
        let mut setup = rusqlite::Connection::open(&path).unwrap();
        let count: i64 = setup
            .query_row(
                "SELECT COUNT(*) FROM records WHERE task_id=?1",
                [&task.id],
                |r| r.get(0),
            )
            .unwrap();
        let tx = setup.transaction().unwrap();
        for index in count..initial_count {
            let result = ResultSubmission {
                id: uuid::Uuid::new_v4().to_string(),
                task_id: task.id.clone(),
                result: ResultInput {
                    attempt_id: aid.into(),
                    summary: format!("Historical result {index}"),
                    artifacts: vec![],
                    evidence: vec![],
                },
                created_at_ms: 5,
            };
            tx.execute("INSERT INTO records(session,kind,id,task_id,body) VALUES('session','result',?1,?2,?3)",rusqlite::params![result.id,task.id,serde_json::to_string(&result).unwrap()]).unwrap();
        }
        tx.commit().unwrap();
        drop(setup);
        let revision = store.detail("session", &task.id).unwrap().task.revision;
        let request = ReconcileInput {
            request_key: "check".into(),
            expected_revision: revision,
            expected_instance_id: None,
            expected_native_owner_epoch: None,
        };
        let prepared = store.prepare_reconciliation("actor", "session", &task.id, aid, request, 6);
        if initial_count == 1022 {
            assert_eq!(prepared.unwrap_err().0, FailureCode::ResourceLimit);
            assert_eq!(
                store
                    .get_receipt("actor", "session", OperationKind::ReconcileAttempt, "check")
                    .unwrap_err()
                    .0,
                FailureCode::NotFound
            );
            assert_eq!(
                store.detail("session", &task.id).unwrap().task.revision,
                revision
            );
            let db = rusqlite::Connection::open(&path).unwrap();
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM records WHERE task_id=?1",
                    [&task.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1022
            );
            drop(db);
        } else {
            let check = prepared.unwrap().value;
            store.begin_operation("session", &check.id, 7).unwrap();
            let revision = store.detail("session", &task.id).unwrap().task.revision;
            store
                .submit_result(
                    "actor",
                    "session",
                    &task.id,
                    "last-result",
                    revision,
                    ResultInput {
                        attempt_id: aid.into(),
                        summary: "Result arriving during native query".into(),
                        artifacts: vec![],
                        evidence: vec![],
                    },
                    8,
                )
                .unwrap();
            store
                .record_start_refusal("session", &start.id, "epoch", "no-process", 9)
                .unwrap();
            let released = finish_check(
                &mut store,
                &task,
                &check,
                ReconciliationEvidence::NativeNotStarted {
                    start_operation_id: start.id.clone(),
                    owner_epoch: "epoch".into(),
                    receipt_id: "no-process".into(),
                },
            )
            .unwrap();
            assert_eq!(released.reservation, Reservation::Released);
            let revision = store.detail("session", &task.id).unwrap().task.revision;
            assert_eq!(
                store
                    .submit_result(
                        "actor",
                        "session",
                        &task.id,
                        "over-limit",
                        revision,
                        ResultInput {
                            attempt_id: aid.into(),
                            summary: "Beyond quota".into(),
                            artifacts: vec![],
                            evidence: vec![]
                        },
                        10
                    )
                    .unwrap_err()
                    .0,
                FailureCode::ResourceLimit
            );
            drop(store);
            store = WorkStore::open(&path).unwrap();
            assert_eq!(
                store
                    .get_reconciliation("session", &check.id)
                    .unwrap()
                    .reservation,
                Reservation::Released
            );
            assert_eq!(
                store
                    .get_start_refusal("session", &start.id)
                    .unwrap()
                    .unwrap()
                    .receipt_id,
                "no-process"
            );
            let db = rusqlite::Connection::open(&path).unwrap();
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM records WHERE task_id=?1",
                    [&task.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1024
            );
            drop(db);
        }
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
fn input() -> CreateTask {
    CreateTask {
        repo_path: "/tmp/project".into(),
        title: "Task".into(),
        brief: "Do the work".into(),
        parent_task_id: None,
        policy: TaskPolicy {
            allowed_agents: vec!["codex".into()],
            max_workers: 2,
        },
    }
}

#[test]
fn parent_pause_fences_prepared_and_future_launches_without_retargeting_work() {
    let mut s = WorkStore::in_memory().unwrap();
    let parent = s.create_task("a", "s", "parent", input(), 1).unwrap().value;
    let mut child_input = input();
    child_input.parent_task_id = Some(parent.id.clone());
    let child = s
        .create_task("a", "s", "child", child_input, 2)
        .unwrap()
        .value;
    let lead = || NewAttempt {
        agent_kind: "codex".into(),
        role: AttemptRole::Lead,
    };
    let pending = s
        .prepare_attempt("a", "s", &child.id, "launch", 1, lead(), "payload", 3)
        .unwrap()
        .value;
    let paused = s
        .set_paused(
            "a",
            "s",
            &parent.id,
            "pause",
            1,
            PauseInput { paused: true },
            4,
        )
        .unwrap()
        .value;
    assert!(paused.paused);
    assert_eq!(
        s.begin_operation("s", &pending.id, 5).unwrap_err().0,
        FailureCode::NotReady
    );
    assert_eq!(
        s.get_operation("s", &pending.id).unwrap().state,
        OperationState::Prepared
    );
    assert_eq!(
        s.prepare_attempt(
            "a",
            "s",
            &parent.id,
            "new",
            paused.revision,
            lead(),
            "payload",
            5
        )
        .unwrap_err()
        .0,
        FailureCode::NotReady
    );
    let replay = s
        .set_paused(
            "a",
            "s",
            &parent.id,
            "pause",
            1,
            PauseInput { paused: true },
            6,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.value.revision, paused.revision);
    assert_eq!(
        s.set_paused(
            "a",
            "s",
            &parent.id,
            "pause",
            1,
            PauseInput { paused: false },
            6
        )
        .unwrap_err()
        .0,
        FailureCode::RequestKeyConflict
    );
    assert_eq!(
        s.set_paused(
            "a",
            "foreign",
            &parent.id,
            "resume",
            paused.revision,
            PauseInput { paused: false },
            6
        )
        .unwrap_err()
        .0,
        FailureCode::NotFound
    );
    s.set_paused(
        "a",
        "s",
        &parent.id,
        "resume",
        paused.revision,
        PauseInput { paused: false },
        7,
    )
    .unwrap();
    assert_eq!(
        s.get_operation("s", &pending.id).unwrap().state,
        OperationState::Prepared
    );
    s.begin_operation("s", &pending.id, 8).unwrap();
}

#[test]
fn pausing_delegation_keeps_existing_attempt_followups_available() {
    let mut s = WorkStore::in_memory().unwrap();
    let task = s.create_task("a", "s", "task", input(), 1).unwrap().value;
    let op = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "launch",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "payload",
            2,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &op.id, 3).unwrap();
    s.finalize_operation(
        "s",
        &op.id,
        OperationOutcome {
            state: OperationState::Acknowledged,
            resources: NativeBinding {
                instance_id: Some("original".into()),
                target: Some("lead".into()),
                ..Default::default()
            },
            failure_code: None,
        },
        4,
    )
    .unwrap();
    let before = s.detail("s", &task.id).unwrap();
    let paused = s
        .set_paused(
            "a",
            "s",
            &task.id,
            "pause",
            before.task.revision,
            PauseInput { paused: true },
            5,
        )
        .unwrap()
        .value;
    let after = s.detail("s", &task.id).unwrap();
    assert_eq!(before.attempts.len(), after.attempts.len());
    assert_eq!(
        after.attempts[0].binding.instance_id.as_deref(),
        Some("original")
    );
    let send = s
        .prepare_delivery(
            "a",
            "s",
            &task.id,
            op.attempt_id.as_deref().unwrap(),
            "followup",
            paused.revision,
            "original",
            "Please report status",
            6,
        )
        .unwrap();
    assert_eq!(send.value.state, OperationState::Prepared);
}
#[test]
fn request_replay_scope_and_conflict() {
    let mut s = WorkStore::in_memory().unwrap();
    let a = s.create_task("a", "s", "key", input(), 1).unwrap();
    let b = s.create_task("a", "s", "key", input(), 2).unwrap();
    assert!(b.replayed);
    assert_eq!(a.value.id, b.value.id);
    let mut changed = input();
    changed.brief = "Different".into();
    assert_eq!(
        s.create_task("a", "s", "key", changed, 2).unwrap_err().0,
        FailureCode::RequestKeyConflict
    );
    assert!(s.detail("other", &a.value.id).is_err());
    assert!(!s.create_task("b", "s", "key", input(), 2).unwrap().replayed);
}
#[test]
fn parent_policy_cannot_widen() {
    let mut s = WorkStore::in_memory().unwrap();
    let p = s.create_task("a", "s", "p", input(), 1).unwrap().value;
    let mut child = input();
    child.parent_task_id = Some(p.id);
    child.policy.allowed_agents.push("claude".into());
    assert_eq!(
        s.create_task("a", "s", "c", child, 1).unwrap_err().0,
        FailureCode::ScopeMismatch
    );
}
#[test]
fn restart_never_replays_input_and_result_reviews_keep_versions() {
    let dir = std::env::temp_dir().join(format!("muqun-work-{}", uuid::Uuid::new_v4()));
    let path = dir.join("work.sqlite");
    let (tid, oid, aid) = {
        let mut s = WorkStore::open(&path).unwrap();
        let t = s.create_task("a", "s", "t", input(), 1).unwrap().value;
        assert_eq!(
            s.prepare_attempt(
                "a",
                "s",
                &t.id,
                "bad",
                0,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead
                },
                "digest",
                2
            )
            .unwrap_err()
            .0,
            FailureCode::RevisionConflict
        );
        let op = s
            .prepare_attempt(
                "a",
                "s",
                &t.id,
                "start",
                1,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "digest",
                2,
            )
            .unwrap()
            .value;
        s.begin_operation("s", &op.id, 3).unwrap();
        (t.id, op.id, op.attempt_id.unwrap())
    };
    let mut s = WorkStore::open(&path).unwrap();
    assert_eq!(
        s.get_operation("s", &oid).unwrap().state,
        OperationState::Unconfirmed
    );
    assert!(s.begin_operation("s", &oid, 4).is_err());
    let revision = s.detail("s", &tid).unwrap().task.revision;
    let result = s
        .submit_result(
            "a",
            "s",
            &tid,
            "result",
            revision,
            ResultInput {
                attempt_id: aid,
                summary: "Ready for review".into(),
                artifacts: vec![ArtifactRef {
                    path: "output.md".into(),
                    sha256: "a".repeat(64),
                    size_bytes: 4,
                }],
                evidence: vec!["Agent-reported test pass".into()],
            },
            5,
        )
        .unwrap()
        .value;
    let rev = s.detail("s", &tid).unwrap().task.revision;
    let review = s
        .review_result(
            "human",
            "s",
            &tid,
            "review",
            rev,
            ReviewInput {
                submission_id: result.id.clone(),
                decision: ReviewDecision::Accepted,
                message: None,
            },
            6,
        )
        .unwrap()
        .value;
    assert_eq!(review.review.submission_id, result.id);
    let detail = s.detail("s", &tid).unwrap();
    assert_eq!(detail.results[0].result.artifacts[0].sha256, "a".repeat(64));
    assert_eq!(detail.cursor, s.changes("s", 0, 100).unwrap().cursor);
    drop(s);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn binding_preconditions_replay_and_immutable_resources() {
    let mut s = WorkStore::in_memory().unwrap();
    let task = s.create_task("a", "s", "t", input(), 1).unwrap().value;
    let op = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "start",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            2,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &op.id, 3).unwrap();
    s.finalize_operation(
        "s",
        &op.id,
        OperationOutcome {
            state: OperationState::Acknowledged,
            resources: NativeBinding {
                instance_id: Some("instance-1".into()),
                target: Some("target".into()),
                pane_id: Some("pane".into()),
                worktree_path: None,
                workspace_id: None,
                tab_id: None,
            },
            failure_code: None,
        },
        4,
    )
    .unwrap();
    let replay = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "start",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            9,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.value.state, OperationState::Acknowledged);
    let rev = s.detail("s", &task.id).unwrap().task.revision;
    let aid = op.attempt_id.unwrap();
    assert_eq!(
        s.prepare_delivery(
            "a",
            "s",
            &task.id,
            &aid,
            "wrong",
            rev,
            "replacement",
            "Hello",
            5
        )
        .unwrap_err()
        .0,
        FailureCode::InstanceChanged
    );
    let delivery = s
        .prepare_delivery(
            "a",
            "s",
            &task.id,
            &aid,
            "send",
            rev,
            "instance-1",
            "Hello",
            5,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &delivery.id, 6).unwrap();
    assert_eq!(
        s.finalize_operation(
            "s",
            &delivery.id,
            OperationOutcome {
                state: OperationState::Acknowledged,
                resources: NativeBinding {
                    instance_id: Some("replacement".into()),
                    ..NativeBinding::default()
                },
                failure_code: None
            },
            7
        )
        .unwrap_err()
        .0,
        FailureCode::InstanceChanged
    );
    assert_eq!(
        s.get_operation("s", &delivery.id).unwrap().state,
        OperationState::Submitting
    );
}

#[test]
fn snapshots_and_lists_are_scoped_and_paginated() {
    let mut s = WorkStore::in_memory().unwrap();
    for key in ["one", "two", "three"] {
        s.create_task("a", "s", key, input(), 1).unwrap();
    }
    s.create_task("a", "other", "one", input(), 1).unwrap();
    let first = s.list_tasks("s", None, 2).unwrap();
    let second = s.list_tasks("s", Some(&first[1].id), 2).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 1);
    let changes = s.changes("s", 0, 2).unwrap();
    assert_eq!(changes.changes.len(), 2);
    let tail = s.changes("s", changes.cursor, 2).unwrap();
    assert_eq!(tail.changes.len(), 1);
    assert!(s.changes("s", u64::MAX, 2).unwrap().reset_required);
}

#[cfg(unix)]
#[test]
fn disk_permissions_and_symlinks_are_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = std::env::temp_dir().join(format!("muqun-work-permissions-{}", uuid::Uuid::new_v4()));
    let path = dir.join("work.sqlite");
    drop(WorkStore::open(&path).unwrap());
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let linked = dir.join("linked.sqlite");
    symlink(&path, &linked).unwrap();
    assert!(WorkStore::open(&linked).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refused_launch_releases_reservation_but_uncertainty_does_not() {
    let mut s = WorkStore::in_memory().unwrap();
    let task = s.create_task("a", "s", "t", input(), 1).unwrap().value;
    let op = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "start",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            2,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &op.id, 3).unwrap();
    s.finalize_operation(
        "s",
        &op.id,
        OperationOutcome {
            state: OperationState::Refused,
            resources: NativeBinding::default(),
            failure_code: Some(FailureCode::NotReady),
        },
        4,
    )
    .unwrap();
    let rev = s.detail("s", &task.id).unwrap().task.revision;
    let next = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "retry",
            rev,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            5,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &next.id, 6).unwrap();
    s.finalize_operation(
        "s",
        &next.id,
        OperationOutcome {
            state: OperationState::Unconfirmed,
            resources: NativeBinding::default(),
            failure_code: Some(FailureCode::DeliveryUnconfirmed),
        },
        7,
    )
    .unwrap();
    let rev = s.detail("s", &task.id).unwrap().task.revision;
    assert_eq!(
        s.prepare_attempt(
            "a",
            "s",
            &task.id,
            "unsafe-retry",
            rev,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead
            },
            "launch",
            8
        )
        .unwrap_err()
        .0,
        FailureCode::ResourceLimit
    );
    assert_eq!(s.detail("s", &task.id).unwrap().attempts.len(), 2);
}

#[test]
fn worker_limit_and_delivery_single_flight() {
    let mut s = WorkStore::in_memory().unwrap();
    let task = s.create_task("a", "s", "t", input(), 1).unwrap().value;
    assert_eq!(
        s.prepare_attempt(
            "a",
            "s",
            &task.id,
            "worker-first",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Worker
            },
            "launch",
            2
        )
        .unwrap_err()
        .0,
        FailureCode::NotReady
    );
    let op = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "lead",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            2,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &op.id, 3).unwrap();
    s.finalize_operation(
        "s",
        &op.id,
        OperationOutcome {
            state: OperationState::Acknowledged,
            resources: NativeBinding {
                instance_id: Some("lead-instance".into()),
                ..NativeBinding::default()
            },
            failure_code: None,
        },
        4,
    )
    .unwrap();
    for key in ["worker1", "worker2"] {
        let rev = s.detail("s", &task.id).unwrap().task.revision;
        s.prepare_attempt(
            "a",
            "s",
            &task.id,
            key,
            rev,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Worker,
            },
            "launch",
            5,
        )
        .unwrap();
    }
    let rev = s.detail("s", &task.id).unwrap().task.revision;
    assert_eq!(
        s.prepare_attempt(
            "a",
            "s",
            &task.id,
            "worker3",
            rev,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Worker
            },
            "launch",
            5
        )
        .unwrap_err()
        .0,
        FailureCode::ResourceLimit
    );
    let aid = op.attempt_id.unwrap();
    let delivery = s
        .prepare_delivery(
            "a",
            "s",
            &task.id,
            &aid,
            "send",
            rev,
            "lead-instance",
            "Hello",
            6,
        )
        .unwrap()
        .value;
    let rev = s.detail("s", &task.id).unwrap().task.revision;
    assert_eq!(
        s.prepare_delivery(
            "a",
            "s",
            &task.id,
            &aid,
            "overlap",
            rev,
            "lead-instance",
            "More",
            7
        )
        .unwrap_err()
        .0,
        FailureCode::NotReady
    );
    s.begin_operation("s", &delivery.id, 8).unwrap();
    s.finalize_operation(
        "s",
        &delivery.id,
        OperationOutcome {
            state: OperationState::Unconfirmed,
            resources: NativeBinding::default(),
            failure_code: Some(FailureCode::DeliveryUnconfirmed),
        },
        9,
    )
    .unwrap();
    let rev = s.detail("s", &task.id).unwrap().task.revision;
    assert_eq!(
        s.prepare_delivery(
            "a",
            "s",
            &task.id,
            &aid,
            "retry-unknown",
            rev,
            "lead-instance",
            "More",
            10
        )
        .unwrap_err()
        .0,
        FailureCode::NotReady
    );
}

#[test]
fn partial_workspace_checkpoint_survives_restart_without_releasing_reservation() {
    let dir = std::env::temp_dir().join(format!("muqun-work-checkpoint-{}", uuid::Uuid::new_v4()));
    let path = dir.join("work.sqlite");
    let (tid, oid) = {
        let mut s = WorkStore::open(&path).unwrap();
        let task = s.create_task("a", "s", "t", input(), 1).unwrap().value;
        let op = s
            .prepare_attempt(
                "a",
                "s",
                &task.id,
                "start",
                1,
                NewAttempt {
                    agent_kind: "codex".into(),
                    role: AttemptRole::Lead,
                },
                "launch",
                2,
            )
            .unwrap()
            .value;
        s.begin_operation("s", &op.id, 3).unwrap();
        let checkpoint = s
            .checkpoint_operation(
                "s",
                &op.id,
                NativeBinding {
                    workspace_id: Some("workspace-1".into()),
                    tab_id: Some("tab-1".into()),
                    ..NativeBinding::default()
                },
                4,
            )
            .unwrap();
        assert_eq!(checkpoint.state, OperationState::Submitting);
        assert_eq!(
            s.checkpoint_operation(
                "s",
                &op.id,
                NativeBinding {
                    workspace_id: Some("workspace-2".into()),
                    ..NativeBinding::default()
                },
                5
            )
            .unwrap_err()
            .0,
            FailureCode::InstanceChanged
        );
        (task.id, op.id)
    };
    let mut s = WorkStore::open(&path).unwrap();
    let op = s.get_operation("s", &oid).unwrap();
    assert_eq!(op.state, OperationState::Unconfirmed);
    assert_eq!(op.resources.workspace_id.as_deref(), Some("workspace-1"));
    let detail = s.detail("s", &tid).unwrap();
    assert_eq!(detail.attempts[0].binding.tab_id.as_deref(), Some("tab-1"));
    drop(s);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn result_receipt_precedes_mutable_artifact_access_and_checks_original_request() {
    let mut s = WorkStore::in_memory().unwrap();
    let task = s.create_task("a", "s", "t", input(), 1).unwrap().value;
    let op = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "start",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            2,
        )
        .unwrap()
        .value;
    let revision = s.detail("s", &task.id).unwrap().task.revision;
    let request = ResultInput {
        attempt_id: op.attempt_id.unwrap(),
        summary: "Immutable result".into(),
        artifacts: vec![ArtifactRef {
            path: "nonexistent-output.md".into(),
            sha256: "a".repeat(64),
            size_bytes: 4,
        }],
        evidence: vec![],
    };
    assert!(s
        .replay_result("a", "s", &task.id, "result", revision, &request)
        .unwrap()
        .is_none());
    let result = s
        .submit_result("a", "s", &task.id, "result", revision, request.clone(), 3)
        .unwrap()
        .value;
    // No source file exists: the receipt is entirely independent of later source availability.
    let replay = s
        .replay_result("a", "s", &task.id, "result", revision, &request)
        .unwrap()
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.value.id, result.id);
    let mut changed = request.clone();
    changed.artifacts[0].sha256 = "b".repeat(64);
    assert_eq!(
        s.replay_result("a", "s", &task.id, "result", revision, &changed)
            .unwrap_err()
            .0,
        FailureCode::RequestKeyConflict
    );
    assert_eq!(
        s.replay_result("a", "s", &task.id, "result", revision + 1, &request)
            .unwrap_err()
            .0,
        FailureCode::RequestKeyConflict
    );
    assert!(s
        .replay_result("other", "s", &task.id, "result", revision, &request)
        .unwrap()
        .is_none());
    assert!(s
        .replay_result("a", "other", &task.id, "result", revision, &request)
        .unwrap()
        .is_none());
}

#[test]
fn child_policy_can_disable_workers_without_disabling_lead() {
    let mut s = WorkStore::in_memory().unwrap();
    let parent = s.create_task("a", "s", "parent", input(), 1).unwrap().value;
    let mut child = input();
    child.parent_task_id = Some(parent.id);
    child.policy.max_workers = 0;
    let task = s.create_task("a", "s", "child", child, 2).unwrap().value;
    let op = s
        .prepare_attempt(
            "a",
            "s",
            &task.id,
            "lead",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "launch",
            3,
        )
        .unwrap()
        .value;
    s.begin_operation("s", &op.id, 4).unwrap();
    s.finalize_operation(
        "s",
        &op.id,
        OperationOutcome {
            state: OperationState::Acknowledged,
            resources: NativeBinding {
                instance_id: Some("lead".into()),
                ..NativeBinding::default()
            },
            failure_code: None,
        },
        5,
    )
    .unwrap();
    let revision = s.detail("s", &task.id).unwrap().task.revision;
    assert_eq!(
        s.prepare_attempt(
            "a",
            "s",
            &task.id,
            "worker",
            revision,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Worker
            },
            "launch",
            6
        )
        .unwrap_err()
        .0,
        FailureCode::ResourceLimit
    );
}

#[cfg(unix)]
#[test]
fn database_fifo_is_refused_without_blocking_or_changing_its_mode() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("work-fifo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("work.sqlite");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated path in this test's private directory.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o640) }, 0);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let worker_path = path.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let worker =
        std::thread::spawn(move || sent.send(WorkStore::open(&worker_path).is_err()).unwrap());
    assert!(received
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("opening a FIFO must not block"));
    worker.join().unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn database_hardlink_does_not_change_external_bytes_or_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("work-hardlink-{}", uuid::Uuid::new_v4()));
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let outside = dir.join("unrelated-file");
    std::fs::write(&outside, b"unrelated bytes").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = state.join("work.sqlite");
    std::fs::hard_link(&outside, &path).unwrap();
    assert!(WorkStore::open(&path).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"unrelated bytes");
    assert_eq!(
        std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn database_parent_symlink_is_refused_but_trusted_ancestor_alias_is_resolved() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = std::env::temp_dir().join(format!("work-parent-{}", uuid::Uuid::new_v4()));
    let real = dir.join("real");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    let alias = dir.join("alias");
    symlink(&real, &alias).unwrap();
    assert!(WorkStore::open(&alias.join("work.sqlite")).is_err());
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o755
    );
    drop(WorkStore::open(&alias.join("private/work.sqlite")).unwrap());
    assert!(real.join("private/work.sqlite").is_file());
    assert_eq!(
        std::fs::metadata(real.join("private"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refusing_unexecuted_intent_never_releases_submitting_reservation() {
    let mut store = WorkStore::in_memory().unwrap();
    let task = store
        .create_task("actor", "session", "task", input(), 1)
        .unwrap()
        .value;
    let attempt = store
        .prepare_attempt(
            "actor",
            "session",
            &task.id,
            "start",
            1,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "digest",
            2,
        )
        .unwrap()
        .value;
    let refused = store
        .refuse_prepared_operation("session", &attempt.id, FailureCode::NotReady, 3)
        .unwrap();
    assert_eq!(refused.state, OperationState::Refused);
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    let next = store
        .prepare_attempt(
            "actor",
            "session",
            &task.id,
            "retry",
            revision,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Lead,
            },
            "digest",
            4,
        )
        .unwrap()
        .value;
    store.begin_operation("session", &next.id, 5).unwrap();
    assert_eq!(
        store
            .refuse_prepared_operation("session", &next.id, FailureCode::NotReady, 6)
            .unwrap_err()
            .0,
        FailureCode::RevisionConflict
    );
    assert_eq!(
        store.get_operation("session", &next.id).unwrap().state,
        OperationState::Submitting
    );
}

fn create_child(store: &mut WorkStore, parent: &Task, key: &str) -> Task {
    let mut request = input();
    request.parent_task_id = Some(parent.id.clone());
    request.policy = parent.policy.clone();
    store
        .create_task("actor", "session", key, request, 1)
        .unwrap()
        .value
}
fn reserve_lead(store: &mut WorkStore, task: &Task, key: &str) -> WorkResult<Mutation<Operation>> {
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    store.prepare_attempt(
        "actor",
        "session",
        &task.id,
        key,
        revision,
        NewAttempt {
            agent_kind: "codex".into(),
            role: AttemptRole::Lead,
        },
        "launch",
        2,
    )
}
#[test]
fn descendants_and_workers_share_ancestor_budget() {
    let mut store = WorkStore::in_memory().unwrap();
    let root = store
        .create_task("actor", "session", "root", input(), 1)
        .unwrap()
        .value;
    let lead = reserve_lead(&mut store, &root, "root-lead").unwrap().value;
    store.begin_operation("session", &lead.id, 3).unwrap();
    store
        .finalize_operation(
            "session",
            &lead.id,
            OperationOutcome {
                state: OperationState::Acknowledged,
                resources: NativeBinding {
                    instance_id: Some("root-lead".into()),
                    ..NativeBinding::default()
                },
                failure_code: None,
            },
            4,
        )
        .unwrap();
    let revision = store.detail("session", &root.id).unwrap().task.revision;
    store
        .prepare_attempt(
            "actor",
            "session",
            &root.id,
            "worker",
            revision,
            NewAttempt {
                agent_kind: "codex".into(),
                role: AttemptRole::Worker,
            },
            "launch",
            5,
        )
        .unwrap();
    let child = create_child(&mut store, &root, "child");
    reserve_lead(&mut store, &child, "child-lead").unwrap();
    let grandchild = create_child(&mut store, &child, "grandchild");
    assert_eq!(
        reserve_lead(&mut store, &grandchild, "grandchild-lead")
            .unwrap_err()
            .0,
        FailureCode::ResourceLimit
    );
    let sibling = create_child(&mut store, &root, "sibling");
    assert_eq!(
        reserve_lead(&mut store, &sibling, "sibling-lead")
            .unwrap_err()
            .0,
        FailureCode::ResourceLimit
    );
}
#[test]
fn zero_delegation_budget_cannot_be_bypassed_with_child_leads() {
    let mut store = WorkStore::in_memory().unwrap();
    let mut request = input();
    request.policy.max_workers = 0;
    let root = store
        .create_task("actor", "session", "root", request, 1)
        .unwrap()
        .value;
    reserve_lead(&mut store, &root, "root-lead").unwrap();
    let child = create_child(&mut store, &root, "child");
    assert_eq!(
        reserve_lead(&mut store, &child, "child-lead")
            .unwrap_err()
            .0,
        FailureCode::ResourceLimit
    );
}
#[test]
fn ancestor_budget_releases_only_definite_empty_refusal() {
    let mut store = WorkStore::in_memory().unwrap();
    let mut request = input();
    request.policy.max_workers = 1;
    let root = store
        .create_task("actor", "session", "root", request, 1)
        .unwrap()
        .value;
    let child = create_child(&mut store, &root, "child");
    let attempt = reserve_lead(&mut store, &child, "first").unwrap().value;
    store
        .refuse_prepared_operation("session", &attempt.id, FailureCode::NotReady, 3)
        .unwrap();
    let sibling = create_child(&mut store, &root, "sibling");
    let second = reserve_lead(&mut store, &sibling, "second").unwrap().value;
    store.begin_operation("session", &second.id, 4).unwrap();
    store
        .finalize_operation(
            "session",
            &second.id,
            OperationOutcome {
                state: OperationState::Unconfirmed,
                resources: NativeBinding::default(),
                failure_code: Some(FailureCode::DeliveryUnconfirmed),
            },
            5,
        )
        .unwrap();
    assert_eq!(
        reserve_lead(&mut store, &child, "retry").unwrap_err().0,
        FailureCode::ResourceLimit
    );
}

#[test]
fn bootstrap_instruction_version_is_chosen_atomically_and_replayed() {
    let mut store = WorkStore::in_memory().unwrap();
    let task = store
        .create_task("actor", "session", "task", input(), 1)
        .unwrap()
        .value;
    let launch = reserve_lead(&mut store, &task, "start").unwrap().value;
    store.begin_operation("session", &launch.id, 2).unwrap();
    store
        .finalize_operation(
            "session",
            &launch.id,
            OperationOutcome {
                state: OperationState::Acknowledged,
                resources: NativeBinding {
                    instance_id: Some("native-launch".into()),
                    ..NativeBinding::default()
                },
                failure_code: None,
            },
            3,
        )
        .unwrap();
    let aid = launch.attempt_id.unwrap();
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    let initial = store
        .prepare_delivery_with_instructions(
            "actor",
            "session",
            &task.id,
            &aid,
            "initial",
            revision,
            "native-launch",
            "User text",
            Some("v1"),
            4,
        )
        .unwrap()
        .value;
    assert_eq!(initial.bootstrap_version.as_deref(), Some("v1"));
    let replay = store
        .prepare_delivery_with_instructions(
            "actor",
            "session",
            &task.id,
            &aid,
            "initial",
            revision,
            "native-launch",
            "User text",
            Some("v1"),
            5,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.value.bootstrap_version, initial.bootstrap_version);
    assert_eq!(
        store
            .prepare_delivery_with_instructions(
                "actor",
                "session",
                &task.id,
                &aid,
                "initial",
                revision,
                "native-launch",
                "User text",
                Some("v2"),
                5
            )
            .unwrap_err()
            .0,
        FailureCode::RequestKeyConflict
    );
    store.begin_operation("session", &initial.id, 6).unwrap();
    store
        .finalize_operation(
            "session",
            &initial.id,
            OperationOutcome {
                state: OperationState::Acknowledged,
                resources: NativeBinding::default(),
                failure_code: None,
            },
            7,
        )
        .unwrap();
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    let followup = store
        .prepare_delivery_with_instructions(
            "actor",
            "session",
            &task.id,
            &aid,
            "followup",
            revision,
            "native-launch",
            "More",
            Some("v1"),
            8,
        )
        .unwrap()
        .value;
    assert!(followup.bootstrap_version.is_none());
    store
        .refuse_prepared_operation("session", &followup.id, FailureCode::InvalidInput, 9)
        .unwrap();
}

#[test]
fn request_receipts_recover_original_creation_and_current_operations_without_mutating() {
    let mut store = WorkStore::in_memory().unwrap();
    let task = store
        .create_task("actor", "session", "create-key", input(), 1)
        .unwrap()
        .value;
    let create = store
        .get_receipt("actor", "session", OperationKind::CreateTask, "create-key")
        .unwrap();
    assert_eq!(create.value["id"], task.id);
    let op = reserve_lead(&mut store, &task, "start-key").unwrap().value;
    let prepared = store
        .get_receipt("actor", "session", OperationKind::StartAttempt, "start-key")
        .unwrap();
    assert_eq!(prepared.value["state"], "prepared");
    store.begin_operation("session", &op.id, 3).unwrap();
    let pending = store
        .get_receipt("actor", "session", OperationKind::StartAttempt, "start-key")
        .unwrap();
    assert_eq!(pending.value["id"], op.id);
    assert_eq!(pending.value["state"], "submitting");
    store
        .finalize_operation(
            "session",
            &op.id,
            OperationOutcome {
                state: OperationState::Unconfirmed,
                resources: NativeBinding::default(),
                failure_code: Some(FailureCode::DeliveryUnconfirmed),
            },
            4,
        )
        .unwrap();
    let finished = store
        .get_receipt("actor", "session", OperationKind::StartAttempt, "start-key")
        .unwrap();
    assert_eq!(finished.value["state"], "unconfirmed");
    let revision = store.detail("session", &task.id).unwrap().task.revision;
    for (actor, session, kind, key) in [
        ("other", "session", OperationKind::CreateTask, "create-key"),
        ("actor", "other", OperationKind::CreateTask, "create-key"),
        (
            "actor",
            "session",
            OperationKind::DeliverPrompt,
            "create-key",
        ),
        ("actor", "session", OperationKind::CreateTask, "missing"),
    ] {
        assert_eq!(
            store.get_receipt(actor, session, kind, key).unwrap_err().0,
            FailureCode::NotFound
        );
    }
    let mut changed = input();
    changed.brief = "Changed payload".into();
    assert_eq!(
        store
            .create_task("actor", "session", "create-key", changed, 5)
            .unwrap_err()
            .0,
        FailureCode::RequestKeyConflict
    );
    let original = store
        .get_receipt("actor", "session", OperationKind::CreateTask, "create-key")
        .unwrap();
    assert_eq!(original.value["revision"], 1);
    assert_eq!(original.value["brief"], input().brief);
    assert_eq!(
        store.detail("session", &task.id).unwrap().task.revision,
        revision
    );
    assert_eq!(
        store
            .get_receipt(
                "actor",
                "session",
                OperationKind::CreateTask,
                &"x".repeat(129)
            )
            .unwrap_err()
            .0,
        FailureCode::InvalidInput
    );
}
