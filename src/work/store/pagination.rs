//! Bounded transport views; internal transactions retain complete task records.
use super::*;
use serde::Deserialize;
use serde_json::Value;

const PAGE_BYTES: usize = 2 * 1024 * 1024;
const PAGE_LIMIT: u32 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub snapshot_revision: u64,
    pub after_id: Option<String>,
    pub next_after_id: Option<String>,
    pub has_more: bool,
}
#[derive(Debug, Serialize)]
pub struct RecordPage {
    pub items: Vec<Value>,
    pub page: Page,
}
#[derive(Debug, Serialize)]
pub struct DetailPages {
    pub attempts: Page,
    pub operations: Page,
    pub results: Page,
    pub reviews: Page,
}
#[derive(Debug, Serialize)]
pub struct PagedDetail {
    #[serde(flatten)]
    pub detail: TaskDetail,
    pub pages: DetailPages,
}

fn read_page(
    db: &Connection,
    session: &str,
    task: &Task,
    kind: &str,
    after: Option<&str>,
    limit: u32,
) -> WorkResult<RecordPage> {
    if !matches!(kind, "attempt" | "operation" | "result" | "review")
        || !(1..=PAGE_LIMIT).contains(&limit)
        || after.is_some_and(|v| uuid::Uuid::parse_str(v).is_err())
    {
        return Err(invalid());
    }
    let mut query = db.prepare("SELECT body FROM records WHERE session=?1 AND task_id=?2 AND kind=?3 AND id>?4 ORDER BY id LIMIT ?5")?;
    let rows = query.query_map(
        params![session, task.id, kind, after.unwrap_or(""), limit + 1],
        |row| row.get::<_, String>(0),
    )?;
    let mut items = Vec::new();
    let mut bytes = 2;
    let mut has_more = false;
    for row in rows {
        let value: Value = serde_json::from_str(&row?)?;
        // Measure wire escaping, not unescaped string lengths.
        let size = serde_json::to_vec(&value)?.len() + 1;
        if items.len() == limit as usize || bytes + size > PAGE_BYTES {
            if items.is_empty() {
                return Err(WorkError(FailureCode::ResourceLimit));
            }
            has_more = true;
            break;
        }
        bytes += size;
        items.push(value);
    }
    let next_after_id = if has_more {
        items
            .last()
            .and_then(|v| v["id"].as_str())
            .map(str::to_owned)
    } else {
        None
    };
    Ok(RecordPage {
        items,
        page: Page {
            snapshot_revision: task.revision,
            after_id: after.map(str::to_owned),
            next_after_id,
            has_more,
        },
    })
}
fn typed<T: DeserializeOwned>(items: Vec<Value>) -> WorkResult<Vec<T>> {
    items
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(Into::into))
        .collect()
}
impl WorkStore {
    pub fn paged_detail(&mut self, session: &str, task_id: &str) -> WorkResult<PagedDetail> {
        let tx = self.connection.transaction()?;
        let task: Task = load(&tx, session, "task", task_id)?;
        let attempts = read_page(&tx, session, &task, "attempt", None, PAGE_LIMIT)?;
        let operations = read_page(&tx, session, &task, "operation", None, PAGE_LIMIT)?;
        let results = read_page(&tx, session, &task, "result", None, PAGE_LIMIT)?;
        let reviews = read_page(&tx, session, &task, "review", None, PAGE_LIMIT)?;
        let value = PagedDetail {
            detail: TaskDetail {
                task,
                attempts: typed(attempts.items)?,
                operations: typed(operations.items)?,
                results: typed(results.items)?,
                reviews: typed(reviews.items)?,
                cursor: cursor(&tx)?,
            },
            pages: DetailPages {
                attempts: attempts.page,
                operations: operations.page,
                results: results.page,
                reviews: reviews.page,
            },
        };
        tx.commit()?;
        Ok(value)
    }
    pub fn task_records(
        &mut self,
        session: &str,
        task_id: &str,
        kind: &str,
        snapshot_revision: u64,
        after_id: Option<&str>,
        limit: u32,
    ) -> WorkResult<RecordPage> {
        let tx = self.connection.transaction()?;
        let task: Task = load(&tx, session, "task", task_id)?;
        revision(&task, snapshot_revision)?;
        let page = read_page(&tx, session, &task, kind, after_id, limit)?;
        tx.commit()?;
        Ok(page)
    }
    pub fn immutable_record(
        &self,
        session: &str,
        task_id: &str,
        kind: &str,
        record_id: &str,
    ) -> WorkResult<Value> {
        if !matches!(kind, "result" | "review") {
            return Err(invalid());
        }
        let value: Value = load(&self.connection, session, kind, record_id)?;
        if value["task_id"].as_str() != Some(task_id) {
            return Err(WorkError(FailureCode::ScopeMismatch));
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_results_remain_readable_and_snapshot_pages_never_skip_history() {
        let mut store = WorkStore::in_memory().unwrap();
        let task = store
            .create_task(
                "human",
                "s",
                "create",
                CreateTask {
                    repo_path: "/tmp/project".into(),
                    title: "Large history".into(),
                    brief: "Review".into(),
                    parent_task_id: None,
                    policy: TaskPolicy {
                        allowed_agents: vec!["codex".into()],
                        max_workers: 1,
                    },
                },
                1,
            )
            .unwrap()
            .value;
        let op = store
            .prepare_attempt(
                "human",
                "s",
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
        let attempt = op.attempt_id.unwrap();
        let mut result_ids = Vec::new();
        for i in 0..24 {
            let revision = store.detail("s", &task.id).unwrap().task.revision;
            let result = store
                .submit_result(
                    "human",
                    "s",
                    &task.id,
                    &format!("result-{i}"),
                    revision,
                    ResultInput {
                        attempt_id: attempt.clone(),
                        summary: "x".repeat(16384),
                        artifacts: vec![],
                        // U+0001 is valid input and expands sixfold in JSON.
                        evidence: vec!["\u{1}".repeat(4096); 32],
                    },
                    i + 3,
                )
                .unwrap()
                .value;
            result_ids.push(result.id);
        }
        let full = store.detail("s", &task.id).unwrap();
        assert!(serde_json::to_vec(&full).unwrap().len() > 16 * 1024 * 1024);
        let first = store.paged_detail("s", &task.id).unwrap();
        assert!(serde_json::to_vec(&first).unwrap().len() < 9 * 1024 * 1024);
        assert!(first.pages.results.has_more);
        let revision = first.detail.task.revision;
        let one = store
            .task_records("s", &task.id, "result", first.detail.task.revision, None, 1)
            .unwrap();
        assert_eq!(one.items.len(), 1);
        assert!(one.page.has_more);
        assert_eq!(
            store
                .task_records(
                    "s",
                    &task.id,
                    "result",
                    first.detail.task.revision,
                    None,
                    21
                )
                .unwrap_err()
                .0,
            FailureCode::InvalidInput
        );
        assert_eq!(
            store
                .task_records(
                    "s",
                    &task.id,
                    "result",
                    first.detail.task.revision,
                    Some("invalid"),
                    20
                )
                .unwrap_err()
                .0,
            FailureCode::InvalidInput
        );

        let mut seen: Vec<_> = first.detail.results.iter().map(|r| r.id.clone()).collect();
        let mut after = first.pages.results.next_after_id;
        while let Some(id) = after {
            let page = store
                .task_records("s", &task.id, "result", revision, Some(&id), 20)
                .unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < PAGE_BYTES + 1024);
            seen.extend(
                page.items
                    .iter()
                    .map(|v| v["id"].as_str().unwrap().to_string()),
            );
            after = page.page.next_after_id;
        }
        result_ids.sort();
        assert_eq!(seen, result_ids);
        let historical = store
            .immutable_record("s", &task.id, "result", &result_ids[0])
            .unwrap();
        assert_eq!(historical["evidence"][0], "\u{1}".repeat(4096));
        assert_eq!(
            store
                .immutable_record("s", "foreign", "result", &result_ids[0])
                .unwrap_err()
                .0,
            FailureCode::ScopeMismatch
        );
        assert_eq!(
            store
                .immutable_record("other", &task.id, "result", &result_ids[0])
                .unwrap_err()
                .0,
            FailureCode::NotFound
        );
        store
            .set_paused(
                "human",
                "s",
                &task.id,
                "pause",
                revision,
                PauseInput { paused: true },
                99,
            )
            .unwrap();
        assert_eq!(
            store
                .task_records("s", &task.id, "result", revision, None, 20)
                .unwrap_err()
                .0,
            FailureCode::RevisionConflict
        );
        assert!(store
            .immutable_record("s", &task.id, "result", &result_ids[0])
            .is_ok());
    }
}
