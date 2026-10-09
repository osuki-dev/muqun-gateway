use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, Transaction};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::domain::{
    now_ms, ChildCounts, Task, TaskBinding, TaskChange, TaskEvent, TaskFilter, TaskId,
    TaskPrincipal, TaskState,
};
use super::ports::{TaskFuture, TaskStore, TaskStoreError};
use crate::platform::store::sqlite::{Migration, SqliteStore};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "tasks",
    steps: &["
CREATE TABLE tasks(
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, goal TEXT NOT NULL, cwd TEXT,
  binding TEXT NOT NULL, state TEXT NOT NULL, summary TEXT, exit_code INTEGER,
  created_by TEXT NOT NULL, created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, seq INTEGER NOT NULL);
CREATE INDEX tasks_by_state ON tasks(state, updated_ms DESC);
CREATE INDEX tasks_by_owner ON tasks(created_by, updated_ms DESC);
CREATE TABLE task_events(task_id TEXT NOT NULL, seq INTEGER NOT NULL, at_ms INTEGER NOT NULL,
  from_state TEXT, to_state TEXT NOT NULL, source TEXT NOT NULL, note TEXT, PRIMARY KEY(task_id, seq));
CREATE TABLE tasks_seq(id INTEGER PRIMARY KEY CHECK(id = 1), value INTEGER NOT NULL);
INSERT INTO tasks_seq(id, value) VALUES(1, 0);
", "
ALTER TABLE tasks ADD COLUMN parent_task_id TEXT;
ALTER TABLE tasks ADD COLUMN origin TEXT NOT NULL DEFAULT 'mcp';
ALTER TABLE tasks ADD COLUMN label TEXT;
ALTER TABLE tasks ADD COLUMN requester_name TEXT;
CREATE INDEX tasks_by_parent ON tasks(parent_task_id, state);
CREATE INDEX tasks_by_asid ON tasks(json_extract(binding, '$.asid'));
"],
};

const DEFAULT_LIST_LIMIT: usize = 100;
const TASK_COLUMNS: &str = "id, kind, goal, cwd, binding, state, summary, exit_code, created_by, created_ms, updated_ms, seq, parent_task_id, origin, label, requester_name";

pub(crate) struct SqliteTaskStore {
    db: Arc<SqliteStore>,
}

impl SqliteTaskStore {
    pub(crate) fn new(db: Arc<SqliteStore>) -> Self {
        Self { db }
    }
}

impl TaskStore for SqliteTaskStore {
    fn insert<'a>(&'a self, task: &'a Task) -> TaskFuture<'a, Task> {
        let task = task.clone();
        Box::pin(self.db.call(move |c| {
            let tx = c.transaction()?;
            let seq = next_seq(&tx)?;
            let mut task = task;
            task.seq = seq;
            tx.execute(
                &format!("INSERT INTO tasks({TASK_COLUMNS}) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"),
                params![
                    task.id.0,
                    enum_text(&task.kind)?,
                    task.goal,
                    task.cwd,
                    json_text(&task.binding)?,
                    enum_text(&task.state)?,
                    task.summary,
                    task.exit_code,
                    json_text(&task.created_by)?,
                    task.created_ms as i64,
                    task.updated_ms as i64,
                    task.seq as i64,
                    task.parent.as_ref().map(|p| &p.0),
                    enum_text(&task.origin)?,
                    task.label,
                    task.requester_name,
                ],
            )?;
            append_event(&tx, &task.id, seq, None, task.state, "created", None)?;
            tx.commit()?;
            Ok(task)
        }))
    }

    fn get<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>> {
        let id = id.clone();
        Box::pin(self.db.call(move |c| load(c, &id)))
    }

    fn latest_device_cwd<'a>(
        &'a self,
        device: &'a TaskPrincipal,
    ) -> TaskFuture<'a, Option<String>> {
        let owner = json_text(device);
        Box::pin(async move {
            let owner = owner?;
            self.db
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT cwd FROM tasks WHERE created_by = ?1 AND origin = 'device' \
                         ORDER BY created_ms DESC, rowid DESC LIMIT 1",
                        [owner],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten())
                })
                .await
        })
    }

    fn list<'a>(&'a self, filter: TaskFilter) -> TaskFuture<'a, Vec<Task>> {
        Box::pin(self.db.call(move |c| {
            let mut sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE 1=1");
            let mut args: Vec<String> = Vec::new();
            if let Some(owner) = &filter.created_by {
                args.push(json_text(owner)?);
                sql.push_str(&format!(" AND created_by = ?{}", args.len()));
            }
            if let Some(states) = &filter.states {
                let mut marks = Vec::new();
                for state in states {
                    args.push(enum_text(state)?);
                    marks.push(format!("?{}", args.len()));
                }
                // An empty list matches nothing, as `IN ()` would.
                let marks = if marks.is_empty() {
                    "NULL".to_string()
                } else {
                    marks.join(", ")
                };
                sql.push_str(&format!(" AND state IN ({marks})"));
            }
            if let Some(root) = &filter.root {
                args.push(root.0.clone());
                sql.push_str(&format!(
                    " AND id IN (WITH RECURSIVE tree(id) AS (SELECT id FROM tasks WHERE id = ?{n} \
                     UNION SELECT t.id FROM tasks t JOIN tree ON t.parent_task_id = tree.id) \
                     SELECT id FROM tree)",
                    n = args.len()
                ));
            }
            if let Some(agent_id) = &filter.agent_id {
                args.push(agent_id.clone());
                sql.push_str(&format!(
                    " AND json_extract(binding, '$.kind') = 'agent_session' \
                     AND json_extract(binding, '$.agent_id') = ?{}",
                    args.len()
                ));
            }
            if let Some(asid) = &filter.asid {
                args.push(asid.clone());
                sql.push_str(&format!(
                    " AND json_extract(binding, '$.asid') = ?{} \
                     AND json_extract(binding, '$.kind') = 'agent_session'",
                    args.len()
                ));
            }
            if let Some((session_id, pane_id)) = &filter.pane {
                args.push(session_id.clone());
                args.push(pane_id.clone());
                sql.push_str(&format!(
                    " AND json_extract(binding, '$.kind') = 'pane' \
                     AND json_extract(binding, '$.session_id') = ?{} \
                     AND json_extract(binding, '$.pane_id') = ?{}",
                    args.len() - 1,
                    args.len()
                ));
            }
            let limit = if filter.limit == 0 {
                DEFAULT_LIST_LIMIT
            } else {
                filter.limit.min(1000)
            };
            sql.push_str(&format!(
                " ORDER BY updated_ms DESC, seq DESC LIMIT {limit}"
            ));
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(args.iter()), read_row)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row??);
            }
            Ok(out)
        }))
    }

    fn transition<'a>(&'a self, id: &'a TaskId, change: TaskChange) -> TaskFuture<'a, Task> {
        let id = id.clone();
        Box::pin(self.db.call(move |c| {
            let tx = c.transaction()?;
            let mut task = load(&tx, &id)?.ok_or(TaskStoreError::NotFound)?;
            if !task.state.can_move_to(change.to) {
                return Err(TaskStoreError::IllegalTransition {
                    from: task.state,
                    to: change.to,
                }
                .into());
            }
            let from = task.state;
            let seq = next_seq(&tx)?;
            task.state = change.to;
            if let Some(summary) = change.summary {
                task.summary = Some(summary);
            }
            if let Some(code) = change.exit_code {
                task.exit_code = Some(code);
            }
            if let Some(binding) = change.binding {
                task.binding = binding;
            }
            task.updated_ms = now_ms();
            task.seq = seq;
            tx.execute(
                "UPDATE tasks SET state=?2, summary=?3, exit_code=?4, binding=?5, updated_ms=?6, seq=?7 WHERE id=?1",
                params![
                    task.id.0,
                    enum_text(&task.state)?,
                    task.summary,
                    task.exit_code,
                    json_text(&task.binding)?,
                    task.updated_ms as i64,
                    task.seq as i64,
                ],
            )?;
            append_event(
                &tx,
                &task.id,
                seq,
                Some(from),
                change.to,
                &change.source,
                change.note.as_deref(),
            )?;
            tx.commit()?;
            Ok(task)
        }))
    }

    fn child_counts<'a>(
        &'a self,
        ids: Vec<TaskId>,
    ) -> TaskFuture<'a, HashMap<TaskId, ChildCounts>> {
        Box::pin(self.db.call(move |c| {
            let mut out: HashMap<TaskId, ChildCounts> = HashMap::new();
            if ids.is_empty() {
                return Ok(out);
            }
            let marks: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
            let mut stmt = c.prepare(&format!(
                "SELECT parent_task_id, state, COUNT(*) FROM tasks WHERE parent_task_id IN ({}) GROUP BY parent_task_id, state",
                marks.join(", ")
            ))?;
            let rows = stmt.query_map(params_from_iter(ids.iter().map(|id| &id.0)), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (parent, state, count) = row?;
                out.entry(TaskId(parent))
                    .or_default()
                    .add(parse_enum(&state)?, count as u32);
            }
            Ok(out)
        }))
    }

    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>> {
        let id = id.clone();
        Box::pin(self.db.call(move |c| {
            let mut stmt = c.prepare(
                "SELECT seq, at_ms, from_state, to_state, source, note FROM task_events WHERE task_id=?1 ORDER BY seq",
            )?;
            let rows = stmt.query_map([&id.0], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (seq, at_ms, from, to, source, note) = row?;
                out.push(TaskEvent {
                    task_id: id.clone(),
                    seq: seq as u64,
                    at_ms: at_ms as u64,
                    from: from.as_deref().map(parse_enum).transpose()?,
                    to: parse_enum(&to)?,
                    source,
                    note,
                });
            }
            Ok(out)
        }))
    }
}

fn next_seq(tx: &Transaction<'_>) -> anyhow::Result<u64> {
    let value: i64 = tx.query_row(
        "UPDATE tasks_seq SET value = value + 1 RETURNING value",
        [],
        |r| r.get(0),
    )?;
    Ok(value as u64)
}

fn append_event(
    tx: &Transaction<'_>,
    id: &TaskId,
    seq: u64,
    from: Option<TaskState>,
    to: TaskState,
    source: &str,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let from = from.map(|s| enum_text(&s)).transpose()?;
    tx.execute(
        "INSERT INTO task_events(task_id, seq, at_ms, from_state, to_state, source, note) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id.0, seq as i64, now_ms() as i64, from, enum_text(&to)?, source, note],
    )?;
    Ok(())
}

fn load(c: &Connection, id: &TaskId) -> anyhow::Result<Option<Task>> {
    c.query_row(
        &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id=?1"),
        [&id.0],
        read_row,
    )
    .optional()?
    .transpose()
}

fn read_row(r: &Row<'_>) -> rusqlite::Result<anyhow::Result<Task>> {
    let id: String = r.get(0)?;
    let kind: String = r.get(1)?;
    let goal: String = r.get(2)?;
    let cwd: Option<String> = r.get(3)?;
    let binding: String = r.get(4)?;
    let state: String = r.get(5)?;
    let summary: Option<String> = r.get(6)?;
    let exit_code: Option<i32> = r.get(7)?;
    let created_by: String = r.get(8)?;
    let created_ms: i64 = r.get(9)?;
    let updated_ms: i64 = r.get(10)?;
    let seq: i64 = r.get(11)?;
    let parent: Option<String> = r.get(12)?;
    let origin: String = r.get(13)?;
    let label: Option<String> = r.get(14)?;
    let requester_name: Option<String> = r.get(15)?;
    Ok((|| {
        Ok(Task {
            id: TaskId(id),
            kind: parse_enum(&kind)?,
            goal,
            cwd,
            binding: serde_json::from_str::<TaskBinding>(&binding)?,
            state: parse_enum(&state)?,
            summary,
            exit_code,
            created_by: serde_json::from_str(&created_by)?,
            created_ms: created_ms as u64,
            updated_ms: updated_ms as u64,
            seq: seq as u64,
            parent: parent.map(TaskId),
            origin: parse_enum(&origin)?,
            label,
            requester_name,
        })
    })())
}

fn json_text<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?)
}

/// A unit enum as its bare snake_case name.
fn enum_text<T: Serialize>(value: &T) -> anyhow::Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => anyhow::bail!("not a plain enum: {other}"),
    }
}

fn parse_enum<T: DeserializeOwned>(text: &str) -> anyhow::Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        text.to_string(),
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::domain::*;
    use crate::tasks::ports::TaskStore;

    fn temp_store(name: &str) -> Arc<SqliteStore> {
        SqliteStore::open(
            &crate::test_state_dir().join(format!("{name}-{}.sqlite3", uuid::Uuid::new_v4())),
            None,
            &[MIGRATION],
        )
        .unwrap()
    }

    fn task(created_by: &str) -> Task {
        Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: "reply PONG".into(),
            cwd: Some("/tmp/p".into()),
            binding: TaskBinding::None,
            state: TaskState::Queued,
            summary: None,
            exit_code: None,
            created_by: TaskPrincipal {
                kind: PrincipalKind::Assistant,
                id: created_by.into(),
            },
            created_ms: 1,
            updated_ms: 1,
            seq: 0,
            parent: None,
            origin: TaskOrigin::Mcp,
            label: None,
            requester_name: None,
        }
    }

    fn change(to: TaskState) -> TaskChange {
        TaskChange {
            to,
            source: "test".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn insert_then_transition_round_trips_and_logs_events() {
        let store = SqliteTaskStore::new(temp_store("tasks"));
        let t = store.insert(&task("pane:a/b")).await.unwrap();
        assert!(t.seq > 0);
        let t = store
            .transition(&t.id, change(TaskState::Dispatching))
            .await
            .unwrap();
        let t = store
            .transition(
                &t.id,
                TaskChange {
                    binding: Some(TaskBinding::AgentSession {
                        agent_id: "opencode".into(),
                        asid: "ses_1".into(),
                    }),
                    ..change(TaskState::Working)
                },
            )
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Working);
        assert!(matches!(t.binding, TaskBinding::AgentSession { .. }));
        let events = store.events(&t.id).await.unwrap();
        assert_eq!(
            events.iter().map(|e| e.to).collect::<Vec<_>>(),
            vec![
                TaskState::Queued,
                TaskState::Dispatching,
                TaskState::Working
            ]
        );
        assert_eq!(events[0].from, None);
        assert_eq!(events[2].from, Some(TaskState::Dispatching));
        assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    }

    #[tokio::test]
    async fn an_illegal_transition_is_refused_and_leaves_the_row() {
        let store = SqliteTaskStore::new(temp_store("illegal"));
        let t = store.insert(&task("pane:a/b")).await.unwrap();
        let err = store
            .transition(&t.id, change(TaskState::Working))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot move"), "{err}");
        assert_eq!(
            store.get(&t.id).await.unwrap().unwrap().state,
            TaskState::Queued
        );
        assert_eq!(store.events(&t.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_task_is_not_found() {
        let store = SqliteTaskStore::new(temp_store("missing"));
        let id = TaskId::new();
        assert!(store.get(&id).await.unwrap().is_none());
        let err = store
            .transition(&id, change(TaskState::Dispatching))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn a_store_from_before_parents_reads_its_rows_as_mcp_roots() {
        let path =
            crate::test_state_dir().join(format!("tasks-v1-{}.sqlite3", uuid::Uuid::new_v4()));
        let v1 = Migration {
            namespace: "tasks",
            steps: &MIGRATION.steps[..1],
        };
        {
            let old = SqliteStore::open(&path, None, &[v1]).unwrap();
            old.call(|c| {
                c.execute(
                    "INSERT INTO tasks(id, kind, goal, cwd, binding, state, summary, exit_code, created_by, created_ms, updated_ms, seq) VALUES('t_old', 'command', 'make', NULL, '{\"kind\":\"none\"}', 'done', 'ok', 0, '{\"kind\":\"assistant\",\"id\":\"a\"}', 1, 1, 1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }
        let store = SqliteTaskStore::new(SqliteStore::open(&path, None, &[MIGRATION]).unwrap());
        let old = store.get(&TaskId("t_old".into())).await.unwrap().unwrap();
        assert_eq!(old.origin, TaskOrigin::Mcp);
        assert_eq!(old.parent, None);
        assert_eq!((old.label, old.requester_name), (None, None));
        assert_eq!(old.summary.as_deref(), Some("ok"));

        let mut child = task("pane:a/b");
        child.parent = Some(old.id.clone());
        child.origin = TaskOrigin::Agent;
        child.label = Some("tests".into());
        let child = store.insert(&child).await.unwrap();
        let read = store.get(&child.id).await.unwrap().unwrap();
        assert_eq!(read.parent, Some(old.id.clone()));
        assert_eq!(read.origin, TaskOrigin::Agent);
        assert_eq!(read.label.as_deref(), Some("tests"));
        let counts = store.child_counts(vec![old.id.clone()]).await.unwrap();
        assert_eq!(counts[&old.id].total, 1);
    }

    /// root -> a -> a1, root -> b; another tree elsewhere.
    #[tokio::test]
    async fn a_root_lists_its_whole_tree_and_counts_direct_children() {
        let store = SqliteTaskStore::new(temp_store("tree"));
        let under = |parent: Option<&Task>| {
            let mut t = task("pane:a/b");
            t.parent = parent.map(|p| p.id.clone());
            t
        };
        let root = store.insert(&under(None)).await.unwrap();
        let a = store.insert(&under(Some(&root))).await.unwrap();
        let b = store.insert(&under(Some(&root))).await.unwrap();
        let a1 = store.insert(&under(Some(&a))).await.unwrap();
        let other = store.insert(&under(None)).await.unwrap();
        store.insert(&under(Some(&other))).await.unwrap();
        store
            .transition(&b.id, change(TaskState::Dispatching))
            .await
            .unwrap();
        store
            .transition(&b.id, change(TaskState::Failed))
            .await
            .unwrap();

        let tree = store
            .list(TaskFilter {
                root: Some(root.id.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut ids: Vec<&str> = tree.iter().map(|t| t.id.0.as_str()).collect();
        ids.sort();
        let mut want = vec![
            root.id.0.as_str(),
            a.id.0.as_str(),
            b.id.0.as_str(),
            a1.id.0.as_str(),
        ];
        want.sort();
        assert_eq!(ids, want);
        let open_only = store
            .list(TaskFilter {
                root: Some(root.id.clone()),
                states: Some(vec![TaskState::Failed]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(open_only.len(), 1);
        assert!(store
            .list(TaskFilter {
                root: Some(TaskId("t_missing".into())),
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());

        let counts = store
            .child_counts(vec![root.id.clone(), a.id.clone(), a1.id.clone()])
            .await
            .unwrap();
        assert_eq!(
            counts[&root.id],
            ChildCounts {
                total: 2,
                active: 1,
                waiting: 0,
                failed: 1
            }
        );
        assert_eq!(counts[&a.id].total, 1);
        assert!(!counts.contains_key(&a1.id), "no children, no entry");
    }

    #[tokio::test]
    async fn tasks_are_found_by_what_they_are_bound_to() {
        let store = SqliteTaskStore::new(temp_store("bound"));
        let bound = |binding: TaskBinding| {
            let mut t = task("pane:a/b");
            t.binding = binding;
            t
        };
        let session = store
            .insert(&bound(TaskBinding::AgentSession {
                agent_id: "opencode".into(),
                asid: "ses_1".into(),
            }))
            .await
            .unwrap();
        let pane = store
            .insert(&bound(TaskBinding::Pane {
                session_id: "default".into(),
                pane_id: "%3".into(),
                tab_id: None,
                shell_pid: Some(9),
                agent_id: None,
            }))
            .await
            .unwrap();
        store.insert(&bound(TaskBinding::None)).await.unwrap();
        let only = |filter: TaskFilter| {
            let store = &store;
            async move {
                store
                    .list(filter)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|t| t.id)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            only(TaskFilter {
                asid: Some("ses_1".into()),
                ..Default::default()
            })
            .await,
            vec![session.id.clone()]
        );
        assert_eq!(
            only(TaskFilter {
                agent_id: Some("opencode".into()),
                ..Default::default()
            })
            .await,
            vec![session.id.clone()]
        );
        assert_eq!(
            only(TaskFilter {
                pane: Some(("default".into(), "%3".into())),
                ..Default::default()
            })
            .await,
            vec![pane.id]
        );
        assert!(only(TaskFilter {
            pane: Some(("other".into(), "%3".into())),
            ..Default::default()
        })
        .await
        .is_empty());
    }

    #[tokio::test]
    async fn list_filters_by_owner_and_state_newest_first() {
        let store = SqliteTaskStore::new(temp_store("list"));
        let mut older = task("pane:a/1");
        older.updated_ms = 10;
        let mut newer = task("pane:a/2");
        newer.updated_ms = 20;
        let a = store.insert(&older).await.unwrap();
        let b = store.insert(&newer).await.unwrap();
        let mine = store
            .list(TaskFilter {
                created_by: Some(a.created_by.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(mine.len(), 1);
        let queued = store
            .list(TaskFilter {
                states: Some(vec![TaskState::Queued]),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            queued.iter().map(|t| &t.id).collect::<Vec<_>>(),
            vec![&b.id, &a.id]
        );
        // A transition bumps updated_ms, which moves the older task to the front.
        store
            .transition(&a.id, change(TaskState::Dispatching))
            .await
            .unwrap();
        let all = store.list(TaskFilter::default()).await.unwrap();
        assert_eq!(all[0].id, a.id);
        assert_eq!(all[1].id, b.id);
        let queued = store
            .list(TaskFilter {
                states: Some(vec![TaskState::Queued]),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(queued.len(), 1);
    }
}
