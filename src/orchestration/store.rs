use rusqlite::{params, OptionalExtension};

use super::{Run, Start};
use crate::platform::store::sqlite::{Migration, SqliteStore};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "orchestration",
    steps: &[
        "CREATE TABLE orchestration_runs (
        id TEXT PRIMARY KEY, owner TEXT NOT NULL, client_ref TEXT NOT NULL,
        request TEXT NOT NULL, created_ms INTEGER NOT NULL, task_id TEXT,
        UNIQUE(owner, client_ref));",
        "ALTER TABLE orchestration_runs ADD COLUMN state_json TEXT;",
    ],
};

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    let request_str: String = row.get(2)?;
    let request: Start = serde_json::from_str(&request_str).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let state_json: Option<String> = row.get(5)?;
    let state = match state_json {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        _ => super::RunState::for_request(&request),
    };
    Ok(Run {
        id: row.get(0)?,
        owner: row.get(1)?,
        request,
        created_ms: row.get::<_, i64>(3)? as u64,
        task_id: row.get(4)?,
        state,
    })
}

/// Reserve before launching anything. Only the caller that inserted may dispatch.
/// Repeated requests return the same run, even after a crash during dispatch.
pub(super) async fn reserve(
    db: &SqliteStore,
    owner: String,
    request: Start,
) -> anyhow::Result<(Run, bool)> {
    db.call(move |db| {
        let id = format!("o_{}", uuid::Uuid::new_v4());
        let now = crate::tasks::domain::now_ms() as i64;
        let initial_state = super::RunState::for_request(&request);
        let state_json = serde_json::to_string(&initial_state)?;
        let fresh = db.execute(
            "INSERT INTO orchestration_runs(id,owner,client_ref,request,created_ms,state_json)
             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(owner,client_ref) DO NOTHING",
            params![
                id,
                owner,
                request.client_ref,
                serde_json::to_string(&request)?,
                now,
                state_json
            ],
        )? == 1;
        let run = db.query_row(
            "SELECT id,owner,request,created_ms,task_id,state_json FROM orchestration_runs WHERE owner=?1 AND client_ref=?2",
            params![owner, request.client_ref],
            row,
        )?;
        Ok((run, fresh))
    })
    .await
}

pub(super) async fn save_state(
    db: &SqliteStore,
    id: String,
    state: super::RunState,
) -> anyhow::Result<()> {
    db.call(move |db| {
        let json = serde_json::to_string(&state)?;
        db.execute(
            "UPDATE orchestration_runs SET state_json=?2 WHERE id=?1",
            params![id, json],
        )?;
        Ok(())
    })
    .await
}

pub(super) async fn read(db: &SqliteStore, id: String) -> anyhow::Result<Option<Run>> {
    db.call(move |db| {
        Ok(db
            .query_row(
                "SELECT id,owner,request,created_ms,task_id,state_json FROM orchestration_runs WHERE id=?1",
                [id],
                row,
            )
            .optional()?)
    })
    .await
}

pub(super) async fn list(db: &SqliteStore) -> anyhow::Result<Vec<Run>> {
    db.call(move |db| {
        let mut query = db.prepare(
            "SELECT id,owner,request,created_ms,task_id,state_json FROM orchestration_runs ORDER BY created_ms DESC,id LIMIT 100",
        )?;
        let rows = query.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

pub(super) async fn by_ref(
    db: &SqliteStore,
    owner: String,
    reference: String,
) -> anyhow::Result<Option<Run>> {
    db.call(move |db| {
        Ok(db
            .query_row(
                "SELECT id,owner,request,created_ms,task_id,state_json FROM orchestration_runs WHERE owner=?1 AND client_ref=?2",
                params![owner, reference],
                row,
            )
            .optional()?)
    })
    .await
}

/// Recover the binding from the task's durable client reference. Never launch a
/// replacement when a dispatch acknowledgement was lost.
pub(super) async fn recover(db: &SqliteStore, run: Run) -> anyhow::Result<Run> {
    db.call(move |db| {
        let mut run = run;
        if run.task_id.is_none() {
            let owner = serde_json::to_string(&crate::tasks::domain::TaskPrincipal {
                kind: crate::tasks::domain::PrincipalKind::Device,
                id: run.owner.clone(),
            })?;
            run.task_id = db
                .query_row(
                    "SELECT id FROM tasks WHERE client_ref=?1 AND created_by=?2 AND parent_task_id IS NULL ORDER BY created_ms LIMIT 1",
                    params![run.task_ref(), owner],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(task) = &run.task_id {
                db.execute(
                    "UPDATE orchestration_runs SET task_id=?2 WHERE id=?1 AND task_id IS NULL",
                    params![run.id, task],
                )?;
            }
        }
        Ok(run)
    })
    .await
}
