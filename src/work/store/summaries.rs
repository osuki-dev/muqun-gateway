use super::*;
const MAX_BYTES: usize = 128 * 1024;

pub(super) fn install(db: &Connection) -> WorkResult<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS work_task_summaries(session TEXT NOT NULL,task_id TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(session,task_id));
        CREATE TABLE IF NOT EXISTS work_task_activity(session TEXT NOT NULL,task_id TEXT NOT NULL,cursor INTEGER NOT NULL,kind TEXT NOT NULL,entity_id TEXT NOT NULL,PRIMARY KEY(session,task_id));
        CREATE TABLE IF NOT EXISTS work_summary_watermarks(session TEXT PRIMARY KEY,cursor INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS work_activity_rebuild ON changes(session,task_id,cursor);
        CREATE INDEX IF NOT EXISTS work_review_submission ON records(session,task_id,json_extract(body,'$.submission_id')) WHERE kind='review';
        INSERT INTO work_summary_watermarks(session,cursor) SELECT session,MAX(cursor) FROM changes GROUP BY session ON CONFLICT(session) DO UPDATE SET cursor=MAX(cursor,excluded.cursor);
        INSERT INTO work_task_activity(session,task_id,cursor,kind,entity_id) SELECT c.session,c.task_id,c.cursor,c.kind,c.entity_id FROM changes c WHERE c.cursor=(SELECT MAX(other.cursor) FROM changes other WHERE other.session=c.session AND other.task_id=c.task_id) ON CONFLICT(session,task_id) DO UPDATE SET cursor=excluded.cursor,kind=excluded.kind,entity_id=excluded.entity_id WHERE excluded.cursor>work_task_activity.cursor;")?;
    Ok(())
}
/// Records are capped at 1024 per task. Recompute only the changed task during its
/// transaction; list reads never deserialize histories or issue per-row queries.
fn refresh(db: &Connection, session: &str, task_id: &str) -> WorkResult<()> {
    let task: Task = load(db, session, "task", task_id)?;
    let activity: Option<TaskActivity> = db
        .query_row(
            "SELECT cursor,kind,entity_id FROM work_task_activity WHERE session=?1 AND task_id=?2",
            params![session, task_id],
            |r| {
                Ok(TaskActivity {
                    cursor: r.get(0)?,
                    kind: r.get(1)?,
                    entity_id: r.get(2)?,
                })
            },
        )
        .optional()?;
    let (reserved,unresolved,unreviewed):(u64,u64,u64)=db.query_row("SELECT
        (SELECT COUNT(*) FROM records WHERE session=?1 AND task_id=?2 AND kind='attempt' AND COALESCE(json_extract(body,'$.lifecycle.reservation'),'reserved')!='released'),
        (SELECT COUNT(*) FROM records o JOIN records a ON a.id=json_extract(o.body,'$.attempt_id') AND a.session=o.session AND a.task_id=o.task_id AND a.kind='attempt' WHERE o.session=?1 AND o.task_id=?2 AND o.kind='operation' AND json_extract(o.body,'$.kind') IN ('start_attempt','deliver_prompt','interrupt_attempt') AND json_extract(o.body,'$.state') IN ('prepared','submitting','unconfirmed') AND COALESCE(json_extract(a.body,'$.lifecycle.reservation'),'reserved')!='released'),
        (SELECT COUNT(*) FROM records result WHERE result.session=?1 AND result.task_id=?2 AND result.kind='result' AND NOT EXISTS(SELECT 1 FROM records review WHERE review.session=result.session AND review.task_id=result.task_id AND review.kind='review' AND json_extract(review.body,'$.submission_id')=result.id))",params![session,task_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    // Record insertion order is the authoritative review order used by the mutation
    // dependency evaluator. Each review and its event commit together; client clocks
    // and UUID order never determine recency, including after SSE cursor pruning.
    let latest:Option<String>=db.query_row("SELECT id FROM records WHERE session=?1 AND task_id=?2 AND kind='result' ORDER BY rowid DESC LIMIT 1",params![session,task_id],|r|r.get(0)).optional()?;
    let latest_result=latest.map(|submission_id| -> WorkResult<SummaryResult> {
        let review:Option<(String,String)>=db.query_row("SELECT id,json_extract(body,'$.decision') FROM records WHERE session=?1 AND task_id=?2 AND kind='review' AND json_extract(body,'$.submission_id')=?3 ORDER BY rowid DESC LIMIT 1",params![session,task_id,submission_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        Ok(SummaryResult{submission_id,review:review.map(|(review_id,decision)|Ok::<_,WorkError>(SummaryReview{review_id,decision:serde_json::from_value(serde_json::Value::String(decision))?})).transpose()?})
    }).transpose()?;
    let summary = TaskSummary {
        task_id: task.id,
        session_id: task.session_id,
        parent_task_id: task.parent_task_id,
        task_revision: task.revision,
        title: task.title,
        repo_path: task.repo_path,
        paused: task.paused,
        last_activity: activity,
        reserved_attempts: reserved,
        unresolved_native_operations: unresolved,
        unreviewed_results: unreviewed,
        latest_result,
    };
    db.execute("INSERT INTO work_task_summaries(session,task_id,body) VALUES(?1,?2,?3) ON CONFLICT(session,task_id) DO UPDATE SET body=excluded.body",params![session,task_id,encoded(&summary)?])?;
    Ok(())
}
pub(super) fn on_change(
    db: &Connection,
    session: &str,
    task_id: &str,
    cursor: i64,
    kind: &str,
    entity: &str,
) -> WorkResult<()> {
    let installed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='work_task_summaries')",[],|r|r.get(0))?;
    if !installed {
        return Ok(());
    }
    db.execute("INSERT INTO work_summary_watermarks(session,cursor) VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET cursor=excluded.cursor",params![session,cursor])?;
    db.execute("INSERT INTO work_task_activity(session,task_id,cursor,kind,entity_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session,task_id) DO UPDATE SET cursor=excluded.cursor,kind=excluded.kind,entity_id=excluded.entity_id",params![session,task_id,cursor,kind,entity])?;
    refresh(db, session, task_id)
}
impl WorkStore {
    pub fn summaries_ready(&self) -> bool {
        self.summaries_ready
    }
    /// Rebuild is bounded in 100-task batches and never enables a partial projection.
    pub fn rebuild_summaries(&mut self) -> WorkResult<()> {
        self.summaries_ready = false;
        install(&self.connection)?;
        let before = cursor(&self.connection)?;
        self.connection
            .execute("DELETE FROM work_task_summaries", [])?;
        let mut after = String::new();
        loop {
            let batch: Vec<(String, String)> = {
                let mut stmt=self.connection.prepare("SELECT session,id FROM records WHERE kind='task' AND id>?1 ORDER BY id LIMIT 100")?;
                let rows = stmt.query_map([&after], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<Result<_, _>>()?
            };
            if batch.is_empty() {
                break;
            }
            let tx = self.connection.transaction()?;
            for (session, task) in &batch {
                refresh(&tx, session, task)?;
            }
            tx.commit()?;
            after = batch.last().ok_or_else(invalid)?.1.clone();
        }
        if cursor(&self.connection)? != before {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        self.summaries_ready = true;
        Ok(())
    }
    pub fn task_summaries(
        &mut self,
        session: &str,
        limit: u32,
        after_id: Option<&str>,
        snapshot_cursor: Option<u64>,
    ) -> WorkResult<TaskSummaryPage> {
        if !self.summaries_ready {
            return Err(WorkError(FailureCode::CapabilityUnavailable));
        }
        if !bounded(session, 256)
            || limit == 0
            || limit > 20
            || snapshot_cursor.is_some_and(|value| value > 9_007_199_254_740_991)
            || after_id.is_some() != snapshot_cursor.is_some()
            || after_id.is_some_and(|s| uuid::Uuid::parse_str(s).is_err())
        {
            return Err(invalid());
        }
        let tx = self.connection.transaction()?;
        let watermark: u64 = tx
            .query_row(
                "SELECT cursor FROM work_summary_watermarks WHERE session=?1",
                [session],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if watermark > 9_007_199_254_740_991 {
            return Err(WorkError(FailureCode::ResourceLimit));
        }
        if snapshot_cursor.is_some_and(|expected| expected != watermark) {
            return Err(WorkError(FailureCode::RevisionConflict));
        }
        let rows: Vec<String> = {
            let mut stmt=tx.prepare("SELECT body FROM work_task_summaries WHERE session=?1 AND task_id>?2 ORDER BY task_id LIMIT ?3")?;
            let rows = stmt
                .query_map(params![session, after_id.unwrap_or(""), limit + 1], |r| {
                    r.get(0)
                })?;
            rows.collect::<Result<_, _>>()?
        };
        let more = rows.len() > limit as usize;
        let mut page = TaskSummaryPage {
            items: vec![],
            snapshot_cursor: watermark,
            next_after_id: None,
        };
        for body in rows.into_iter().take(limit as usize) {
            let item: TaskSummary = serde_json::from_str(&body)?;
            if item.session_id != session
                || item.task_revision > 9_007_199_254_740_991
                || item.reserved_attempts > 1024
                || item.unresolved_native_operations > 1024
                || item.unreviewed_results > 1024
                || item
                    .last_activity
                    .as_ref()
                    .is_some_and(|event| event.cursor > watermark)
            {
                return Err(WorkError(FailureCode::StorageUnavailable));
            }
            page.items.push(item);
            page.next_after_id = page.items.last().map(|item| item.task_id.clone());
            if serde_json::to_vec(&page)?.len() > MAX_BYTES {
                page.items.pop();
                if page.items.is_empty() {
                    return Err(WorkError(FailureCode::ResourceLimit));
                }
                page.next_after_id = page.items.last().map(|item| item.task_id.clone());
                tx.commit()?;
                return Ok(page);
            }
        }
        if !more {
            page.next_after_id = None;
        }
        tx.commit()?;
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn task(store: &mut WorkStore, session: &str, path: &str) -> Task {
        store
            .create_task(
                "device",
                session,
                &id(),
                CreateTask {
                    repo_path: path.into(),
                    title: "Task".into(),
                    brief: "Never in summary".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 0,
                    },
                },
                1,
            )
            .unwrap()
            .value
    }
    fn revise(store: &mut WorkStore, task: &Task, kind: &str, entity: &str) {
        let tx = store.connection.transaction().unwrap();
        let mut current: Task = load(&tx, &task.session_id, "task", &task.id).unwrap();
        changed(&tx, &mut current, kind, entity, 1).unwrap();
        tx.commit().unwrap();
    }
    fn summary(store: &mut WorkStore) -> TaskSummary {
        store
            .task_summaries("s", 20, None, None)
            .unwrap()
            .items
            .remove(0)
    }
    #[test]
    fn projection_tracks_exact_result_review_order_and_lifecycle_without_completion() {
        let mut store = WorkStore::in_memory().unwrap();
        let task = task(&mut store, "s", "/repo");
        let mut attempt = Attempt {
            id: id(),
            task_id: task.id.clone(),
            agent_kind: "codex".into(),
            role: AttemptRole::Lead,
            binding: Default::default(),
            lifecycle: Default::default(),
            created_at_ms: 1,
        };
        put(
            &store.connection,
            "s",
            "attempt",
            &attempt.id,
            &task.id,
            &attempt,
        )
        .unwrap();
        for kind in [
            OperationKind::StartAttempt,
            OperationKind::DeliverPrompt,
            OperationKind::InterruptAttempt,
            OperationKind::ReconcileAttempt,
        ] {
            let mut op = new_operation(&task.id, Some(attempt.id.clone()), kind, 1);
            op.state = OperationState::Unconfirmed;
            put(&store.connection, "s", "operation", &op.id, &task.id, &op).unwrap();
        }
        let result = ResultSubmission {
            id: id(),
            task_id: task.id.clone(),
            result: ResultInput {
                attempt_id: attempt.id.clone(),
                summary: "Private result".into(),
                artifacts: vec![],
                evidence: vec![],
            },
            created_at_ms: 100,
        };
        put(
            &store.connection,
            "s",
            "result",
            &result.id,
            &task.id,
            &result,
        )
        .unwrap();
        revise(&mut store, &task, "result_submitted", &result.id);
        let first = summary(&mut store);
        assert_eq!(first.reserved_attempts, 1);
        assert_eq!(first.unresolved_native_operations, 3);
        assert_eq!(first.unreviewed_results, 1);
        let mut last_review = String::new();
        for (decision, time) in [
            (ReviewDecision::Accepted, 1000),
            (ReviewDecision::ChangesRequested, 1),
        ] {
            let review = Review {
                id: id(),
                task_id: task.id.clone(),
                actor_id: "device".into(),
                review: ReviewInput {
                    submission_id: result.id.clone(),
                    decision,
                    message: None,
                },
                created_at_ms: time,
            };
            put(
                &store.connection,
                "s",
                "review",
                &review.id,
                &task.id,
                &review,
            )
            .unwrap();
            revise(&mut store, &task, "result_reviewed", &review.id);
            last_review = review.id;
        }
        let reviewed = summary(&mut store);
        assert_eq!(reviewed.unreviewed_results, 0);
        let review = reviewed.latest_result.unwrap().review.unwrap();
        assert_eq!(review.review_id, last_review);
        assert_eq!(review.decision, ReviewDecision::ChangesRequested);
        let mut newer = result;
        newer.id = id();
        newer.created_at_ms = 0;
        put(
            &store.connection,
            "s",
            "result",
            &newer.id,
            &task.id,
            &newer,
        )
        .unwrap();
        revise(&mut store, &task, "result_submitted", &newer.id);
        let latest = summary(&mut store);
        assert_eq!(latest.unreviewed_results, 1);
        assert_eq!(
            latest.latest_result.unwrap(),
            SummaryResult {
                submission_id: newer.id,
                review: None
            }
        );
        attempt.lifecycle.reservation = Reservation::Released;
        put(
            &store.connection,
            "s",
            "attempt",
            &attempt.id,
            &task.id,
            &attempt,
        )
        .unwrap();
        revise(&mut store, &task, "attempt_reconciled", &attempt.id);
        let released = summary(&mut store);
        assert_eq!(released.reserved_attempts, 0);
        assert_eq!(released.unresolved_native_operations, 0);
        let before = store.task_summaries("s", 20, None, None).unwrap();
        store.rebuild_summaries().unwrap();
        assert_eq!(store.task_summaries("s", 20, None, None).unwrap(), before);
        let json = encoded(&before).unwrap();
        assert!(!json.contains("Private result"));
        assert!(!json.contains("Never in summary"));
    }
    #[test]
    fn summary_pages_are_bounded_session_pinned_and_survive_pruned_history_rebuild() {
        let dir = std::env::temp_dir().join(format!("summaries-{}", id()));
        let path = dir.join("work.sqlite");
        let mut store = WorkStore::open(&path).unwrap();
        for _ in 0..23 {
            task(&mut store, "s", &format!("/{}", "\"".repeat(4094)));
        }
        let first = store.task_summaries("s", 20, None, None).unwrap();
        assert!(first.items.len() < 20);
        assert!(encoded(&first).unwrap().len() <= MAX_BYTES);
        assert!(first.next_after_id.is_some());
        task(&mut store, "other", "/foreign");
        let second = store
            .task_summaries(
                "s",
                20,
                first.next_after_id.as_deref(),
                Some(first.snapshot_cursor),
            )
            .unwrap();
        assert!(second.items.iter().all(|row| row.session_id == "s"));
        assert!(first
            .items
            .iter()
            .all(|a| second.items.iter().all(|b| a.task_id != b.task_id)));
        assert!(store.task_summaries("s", 21, None, None).is_err());
        assert!(store
            .task_summaries("s", 20, first.next_after_id.as_deref(), None)
            .is_err());
        let snapshot = store.task_summaries("s", 20, None, None).unwrap();
        store.connection.execute("DELETE FROM changes", []).unwrap();
        store.rebuild_summaries().unwrap();
        assert_eq!(store.task_summaries("s", 20, None, None).unwrap(), snapshot);
        drop(store);
        let mut store = WorkStore::open(&path).unwrap();
        assert_eq!(store.task_summaries("s", 20, None, None).unwrap(), snapshot);
        task(&mut store, "s", "/new");
        assert_eq!(
            store
                .task_summaries(
                    "s",
                    20,
                    first.next_after_id.as_deref(),
                    Some(first.snapshot_cursor)
                )
                .unwrap_err()
                .0,
            FailureCode::RevisionConflict
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn projection_rollback_and_failed_rebuild_do_not_disable_legacy_reads() {
        let mut store = WorkStore::in_memory().unwrap();
        let task = task(&mut store, "s", "/repo");
        let before = summary(&mut store);
        {
            let tx = store.connection.transaction().unwrap();
            let mut task = task.clone();
            task.paused = true;
            changed(&tx, &mut task, "delegation_paused", &before.task_id, 2).unwrap();
            tx.rollback().unwrap();
        }
        assert_eq!(summary(&mut store), before);
        store.connection.execute("CREATE TRIGGER fail_projection BEFORE INSERT ON work_task_summaries BEGIN SELECT RAISE(ABORT,'test projection failure'); END",[]).unwrap();
        assert!(store.rebuild_summaries().is_err());
        assert!(!store.summaries_ready());
        assert_eq!(store.list_tasks("s", None, 20).unwrap().len(), 1);
        assert_eq!(
            store.task_summaries("s", 20, None, None).unwrap_err().0,
            FailureCode::CapabilityUnavailable
        );
        store
            .connection
            .execute("DROP TRIGGER fail_projection", [])
            .unwrap();
        store.rebuild_summaries().unwrap();
        assert_eq!(summary(&mut store), before);
        store.connection.execute_batch("DROP TABLE work_task_summaries;DROP TABLE work_task_activity;DROP TABLE work_summary_watermarks;").unwrap();
        store.rebuild_summaries().unwrap();
        assert_eq!(summary(&mut store), before);
    }
}
