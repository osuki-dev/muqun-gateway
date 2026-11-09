//! The search index: a SQLite FTS5 table with the trigram tokenizer, in its
//! own file beside the shared store (`store/search.sqlite3`), keyed like it
//! when the store is encrypted. Derived data only: a file that does not open
//! is deleted and rebuilt.

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, ToSql};

use super::text::{like_pattern, row_hash, Query, Row};
use crate::platform::store::sqlite::Migration;

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "search",
    steps: &[r#"
CREATE TABLE search_session(
    agent_id TEXT NOT NULL,
    asid TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    directory TEXT,
    parent_asid TEXT,
    updated_ms INTEGER NOT NULL DEFAULT 0,
    watermark TEXT,
    indexed_ms INTEGER,
    PRIMARY KEY(agent_id, asid)
) WITHOUT ROWID;
CREATE TABLE search_item(
    id INTEGER PRIMARY KEY,
    agent_id TEXT NOT NULL,
    asid TEXT NOT NULL,
    item_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    role TEXT NOT NULL,
    kind TEXT NOT NULL,
    time_ms INTEGER NOT NULL,
    hash INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE(agent_id, asid, item_id)
);
CREATE INDEX search_item_time ON search_item(time_ms);
CREATE VIRTUAL TABLE search_fts USING fts5(
    text, content='search_item', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER search_item_ai AFTER INSERT ON search_item BEGIN
    INSERT INTO search_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER search_item_ad AFTER DELETE ON search_item BEGIN
    INSERT INTO search_fts(search_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
"#],
};

/// What a session is listed as.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SessionMeta {
    pub agent_id: String,
    pub asid: String,
    pub title: String,
    pub directory: Option<String>,
    pub parent_asid: Option<String>,
    pub updated_ms: u64,
}

/// How a batch of rows meets what is indexed for its session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// The rows are the whole session: anything else indexed for it goes.
    Replace,
    /// The rows are new or changed; the rest stay.
    Merge,
}

/// What [`apply`] changed, for the build's own accounting and the tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub inserted: usize,
    pub deleted: usize,
}

pub(crate) fn apply(
    c: &mut Connection,
    meta: &SessionMeta,
    rows: &[Row],
    mode: Mode,
    watermark: Option<&str>,
    now_ms: u64,
) -> rusqlite::Result<Applied> {
    let tx = c.transaction()?;
    upsert_meta(&tx, meta)?;
    let mut existing: HashMap<String, (i64, i64)> = HashMap::new();
    {
        let mut select = tx.prepare_cached(
            "SELECT item_id, id, hash FROM search_item WHERE agent_id=?1 AND asid=?2",
        )?;
        let found = select.query_map(params![meta.agent_id, meta.asid], |r| {
            Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
        })?;
        for row in found {
            let (item, ids) = row?;
            existing.insert(item, ids);
        }
    }
    let mut applied = Applied::default();
    let kept: HashSet<&str> = rows.iter().map(|row| row.item_id.as_str()).collect();
    {
        let mut delete = tx.prepare_cached("DELETE FROM search_item WHERE id=?1")?;
        let mut insert = tx.prepare_cached(
            "INSERT INTO search_item(agent_id, asid, item_id, message_id, role, kind, time_ms, hash, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        // A batch may hold a streamed row more than once; the last wins.
        let mut written: HashSet<&str> = HashSet::new();
        for row in rows.iter().rev() {
            if !written.insert(row.item_id.as_str()) {
                continue;
            }
            let hash = row_hash(row);
            match existing.get(&row.item_id) {
                Some((_, known)) if *known == hash => continue,
                Some((id, _)) => {
                    delete.execute([id])?;
                    applied.deleted += 1;
                }
                None => {}
            }
            insert.execute(params![
                meta.agent_id,
                meta.asid,
                row.item_id,
                row.message_id,
                row.role,
                row.kind,
                row.time_ms as i64,
                hash,
                row.text
            ])?;
            applied.inserted += 1;
        }
        if mode == Mode::Replace {
            for (item, (id, _)) in &existing {
                if !kept.contains(item.as_str()) {
                    delete.execute([id])?;
                    applied.deleted += 1;
                }
            }
        }
    }
    tx.execute(
        "UPDATE search_session SET watermark=?3, indexed_ms=?4 WHERE agent_id=?1 AND asid=?2",
        params![meta.agent_id, meta.asid, watermark, now_ms as i64],
    )?;
    tx.commit()?;
    Ok(applied)
}

fn upsert_meta(c: &Connection, meta: &SessionMeta) -> rusqlite::Result<()> {
    c.prepare_cached(
        "INSERT INTO search_session(agent_id, asid, title, directory, parent_asid, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, asid) DO UPDATE SET title=excluded.title,
           directory=coalesce(excluded.directory, directory),
           parent_asid=coalesce(excluded.parent_asid, parent_asid),
           updated_ms=max(updated_ms, excluded.updated_ms)",
    )?
    .execute(params![
        meta.agent_id,
        meta.asid,
        meta.title,
        meta.directory,
        meta.parent_asid,
        meta.updated_ms as i64
    ])?;
    Ok(())
}

pub(crate) fn remove_items(
    c: &mut Connection,
    agent_id: &str,
    asid: &str,
    ids: &[String],
) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    {
        let mut delete = tx.prepare_cached(
            "DELETE FROM search_item WHERE agent_id=?1 AND asid=?2 AND item_id=?3",
        )?;
        for id in ids {
            delete.execute(params![agent_id, asid, id])?;
        }
    }
    tx.commit()
}

pub(crate) fn delete_session(
    c: &mut Connection,
    agent_id: &str,
    asid: &str,
) -> rusqlite::Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "DELETE FROM search_item WHERE agent_id=?1 AND asid=?2",
        params![agent_id, asid],
    )?;
    tx.execute(
        "DELETE FROM search_session WHERE agent_id=?1 AND asid=?2",
        params![agent_id, asid],
    )?;
    tx.commit()
}

/// Every indexed session's metadata and watermark.
pub(crate) fn sessions(c: &Connection) -> rusqlite::Result<Vec<(SessionMeta, Option<String>)>> {
    let mut select = c.prepare(
        "SELECT agent_id, asid, title, directory, parent_asid, updated_ms, watermark FROM search_session",
    )?;
    let rows = select.query_map([], |r| {
        Ok((
            SessionMeta {
                agent_id: r.get(0)?,
                asid: r.get(1)?,
                title: r.get(2)?,
                directory: r.get(3)?,
                parent_asid: r.get(4)?,
                updated_ms: r.get::<_, i64>(5)?.max(0) as u64,
            },
            r.get(6)?,
        ))
    })?;
    rows.collect()
}

/// The filters a search takes besides its text.
#[derive(Debug, Clone, Default)]
pub(crate) struct Filters {
    pub agent_id: Option<String>,
    /// The directory or any directory below it.
    pub directory: Option<String>,
}

/// One matching row with its session.
#[derive(Debug, Clone)]
pub(crate) struct Hit {
    pub meta: SessionMeta,
    pub item_id: String,
    pub message_id: String,
    pub role: String,
    pub kind: String,
    pub time_ms: u64,
    pub text: String,
    /// FTS5 `bm25()`, lower is better; `None` for a short-terms-only scan.
    pub bm25: Option<f64>,
}

/// At most `candidates` rows matching every term of `query`: by bm25 when
/// a term is long enough for the trigram index, newest first otherwise.
pub(crate) fn search(
    c: &Connection,
    query: &Query,
    filters: &Filters,
    candidates: usize,
) -> rusqlite::Result<Vec<Hit>> {
    let fts = query.fts_expression();
    let mut sql = String::from(
        "SELECT s.agent_id, s.asid, s.title, s.directory, s.parent_asid, s.updated_ms,
                i.item_id, i.message_id, i.role, i.kind, i.time_ms, i.text, ",
    );
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if let Some(expression) = &fts {
        sql.push_str(
            "bm25(search_fts) FROM search_fts JOIN search_item i ON i.id = search_fts.rowid
             JOIN search_session s ON s.agent_id = i.agent_id AND s.asid = i.asid
             WHERE search_fts MATCH ?",
        );
        args.push(Box::new(expression.clone()));
    } else {
        sql.push_str(
            "NULL FROM search_item i
             JOIN search_session s ON s.agent_id = i.agent_id AND s.asid = i.asid
             WHERE 1",
        );
    }
    for term in query.short() {
        sql.push_str(" AND i.text LIKE ? ESCAPE '\\'");
        args.push(Box::new(like_pattern(term)));
    }
    if let Some(agent) = &filters.agent_id {
        sql.push_str(" AND i.agent_id = ?");
        args.push(Box::new(agent.clone()));
    }
    if let Some(directory) = &filters.directory {
        let directory = directory.trim_end_matches('/');
        sql.push_str(" AND (s.directory = ? OR s.directory LIKE ? ESCAPE '\\')");
        args.push(Box::new(directory.to_owned()));
        let mut below = like_pattern(directory);
        below.remove(0);
        below.pop();
        args.push(Box::new(format!("{below}/%")));
    }
    sql.push_str(if fts.is_some() {
        " ORDER BY bm25(search_fts) LIMIT ?"
    } else {
        " ORDER BY i.time_ms DESC LIMIT ?"
    });
    args.push(Box::new(candidates as i64));
    let mut select = c.prepare(&sql)?;
    let hits = select.query_map(
        rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
        |r| {
            Ok(Hit {
                meta: SessionMeta {
                    agent_id: r.get(0)?,
                    asid: r.get(1)?,
                    title: r.get(2)?,
                    directory: r.get(3)?,
                    parent_asid: r.get(4)?,
                    updated_ms: r.get::<_, i64>(5)?.max(0) as u64,
                },
                item_id: r.get(6)?,
                message_id: r.get(7)?,
                role: r.get(8)?,
                kind: r.get(9)?,
                time_ms: r.get::<_, i64>(10)?.max(0) as u64,
                text: r.get(11)?,
                bm25: r.get(12)?,
            })
        },
    )?;
    hits.collect()
}
