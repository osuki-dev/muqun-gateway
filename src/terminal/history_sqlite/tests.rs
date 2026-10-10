use super::super::backend::{AgentStatus, Pane, PaneId, TabId, WorkspaceId};
use super::super::history::read_page;
use super::*;
use crate::platform::store::db_key::DbKey;
use crate::platform::store::sqlite::ALL_MIGRATIONS;
use serde_json::json;
use std::path::PathBuf;

#[tokio::test]
async fn migration_failure_rolls_back_schema_and_version() {
    let fixture = Fixture::new();
    let path = fixture.db_path();
    // A table already holding one of the namespace's names fails its step.
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE history_captures(x)")
        .unwrap();
    assert!(SqliteStore::open(&path, None, &[MIGRATION]).is_err());
    let connection = Connection::open(&path).unwrap();
    let tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='history_backends'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the failed step left no table behind");
    let versions: i64 = connection
        .query_row(
            "SELECT count(*) FROM muqun_schema WHERE namespace='history'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(versions, 0, "the failed step recorded no version");
    connection
        .execute_batch("DROP TABLE history_captures")
        .unwrap();
    drop(connection);
    let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
    let version: i64 = db
        .call(|c| {
            Ok(c.query_row(
                "SELECT version FROM muqun_schema WHERE namespace='history'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(version, MIGRATION.steps.len() as i64);
    let indexes: Vec<String> = db
        .call(|c| {
            Ok(c.prepare("SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='history_captures' AND sql IS NOT NULL ORDER BY name")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(indexes, ["history_capture_quota"]);
}

struct Fixture {
    directory: PathBuf,
    config: Config,
    /// One key for every reopen, as production keeps `store.key`.
    key: DbKey,
}
impl Fixture {
    fn new() -> Self {
        let directory =
            crate::test_state_dir().join(format!("history-sqlite-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut config = crate::test_config("fixture-only");
        config.history.storage = HistoryStorage::Sqlite;
        config.sessions[0].socket_path = directory
            .join("isolated.sock")
            .to_string_lossy()
            .into_owned();
        Self {
            directory,
            config,
            key: DbKey::random(),
        }
    }
    fn db_path(&self) -> PathBuf {
        self.directory.join("gateway.sqlite3")
    }
    /// A fresh handle on the same file, as a restarted gateway opens it:
    /// every domain's migrations, encrypted under the fixture's one key.
    fn db(&self) -> Arc<SqliteStore> {
        SqliteStore::open(&self.db_path(), Some(self.key.clone()), ALL_MIGRATIONS).unwrap()
    }
    /// Writes on every pass: the debounce has its own test.
    async fn open(&self, captures: Arc<Mutex<ScrollbackStore>>) -> SqliteHistoryRepository {
        SqliteHistoryRepository::open_with(self.db(), &self.config, captures, Duration::ZERO).await
    }
    /// The write side alone, driven by explicit passes instead of the tick.
    async fn checkpoints(&self, captures: Arc<Mutex<ScrollbackStore>>) -> Arc<Checkpoints> {
        self.checkpoints_with(captures, Duration::ZERO).await
    }
    async fn checkpoints_with(
        &self,
        captures: Arc<Mutex<ScrollbackStore>>,
        debounce: Duration,
    ) -> Arc<Checkpoints> {
        Checkpoints::open(self.db(), &self.config, captures, debounce)
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn sql_i64(db: &SqliteStore, sql: &'static str) -> i64 {
    db.call(move |c| Ok(c.query_row(sql, [], |row| row.get(0))?))
        .await
        .unwrap()
}

async fn record_count(checkpoints: &Checkpoints) -> i64 {
    sql_i64(&checkpoints.db, "SELECT count(*) FROM history_captures").await
}

async fn passes(checkpoints: &Arc<Checkpoints>, now: i64) {
    for _ in 0..4 {
        checkpoints.flush_at(now).await.unwrap();
    }
}

async fn updated(checkpoints: &Checkpoints, key: String) -> i64 {
    checkpoints
        .db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT updated FROM history_captures WHERE identity=?1",
                [key],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap()
}

fn pane(identity: Option<&str>) -> Pane {
    Pane {
        history_identity: identity.map(str::to_owned),
        id: PaneId::new("p"),
        terminal_id: Some("terminal".into()),
        workspace_id: WorkspaceId::new("w"),
        tab_id: TabId::new("t"),
        label: None,
        terminal_title: None,
        cwd: None,
        focused: false,
        width: Some(80),
        height: Some(4),
        revision: None,
        foreground_command: Some("claude".into()),
        agent: None,
        agent_status: AgentStatus::Unknown,
        max_offset_from_bottom: Some(0),
        viewport_rows: Some(4),
        alternate_on: Some(true),
        cursor_x: None,
        cursor_y: None,
        shell_pid: None,
    }
}
fn scope() -> HistoryScope {
    HistoryScope {
        session: "default".into(),
        pane: "p".into(),
        format: "text".into(),
        device: "d".into(),
        generation: "g".into(),
        limit: 2,
    }
}
fn captures(identity: Option<&str>, feed: bool) -> Arc<Mutex<ScrollbackStore>> {
    let mut store = ScrollbackStore::default();
    store.observe_native_listing("default", &[pane(identity)]);
    if feed {
        for top in 0..8 {
            store.record_frame(
                "default",
                "p",
                "recent_unwrapped",
                "text",
                &(top..top + 4)
                    .map(|n| format!("row {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    Arc::new(Mutex::new(store))
}

fn feed_ansi(store: &mut ScrollbackStore) {
    for top in 0..8 {
        store.record_frame("default", "p", "recent_unwrapped", "ansi", &ansi_frame(top));
    }
}

fn ansi_frame(top: usize) -> String {
    (top..top + 4)
        .map(|n| format!("\u{1b}[31mrow {n}\u{1b}[0m"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn numbered_frame(from: usize, count: usize, size: usize, format: &str) -> String {
    (from..from + count)
        .map(|n| {
            let row = format!("{n:08} {}", "x".repeat(size - 9));
            if format == "ansi" {
                format!("\u{1b}[31m{row}\u{1b}[0m")
            } else {
                row
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn sqlite_byte_and_line_caps_slide_cursors_and_recover_only_the_retained_projection() {
    for format in ["text", "ansi"] {
        for byte_cap in [true, false] {
            let fixture = Fixture::new();
            let fold = captures(Some("A"), false);
            let mut scope = scope();
            scope.format = format.into();
            scope.limit = 500;
            let size = if byte_cap { 1024 } else { 32 };
            let count = if byte_cap {
                MAX_PANE_BYTES / size + 200
            } else {
                MAX_PANE_LINES + 200
            };
            let frame = |from, count| numbered_frame(from, count, size, format);
            let feed = |text: &str| {
                fold.lock()
                    .unwrap()
                    .record_frame("default", "p", "recent_unwrapped", format, text);
            };
            feed(&frame(0, count));
            feed(&frame(500_000, 4));
            let repository = fixture.open(fold.clone()).await;
            // A cap-full projection must be writable, not a healthy memory
            // page masking a write failure caused by encoding overhead.
            persisted_scope(&repository, &scope).await;
            let before = repository.capture(&scope, true).await.unwrap().unwrap();
            let metadata = repository.capture(&scope, false).await.unwrap().unwrap();
            assert_eq!(metadata.len, before.rows.len());
            assert_eq!(metadata.trimmed, before.trimmed);
            assert!(metadata.rows.is_empty());
            assert!(before.trimmed > 0);
            if byte_cap {
                assert!(encode_rows(&before.rows).len() > MAX_PANE_BYTES);
            } else {
                assert_eq!(before.len, MAX_PANE_LINES - 4);
            }

            let first = read_page(&repository, scope.clone(), None).await.unwrap();
            assert_eq!(
                read_page(&repository, scope.clone(), None).await.unwrap(),
                first
            );
            let mut page = first.clone();
            let mut cursors = Vec::new();
            while let Some(cursor) = page["next_before"].as_str().map(str::to_owned) {
                page = read_page(&repository, scope.clone(), Some(&cursor))
                    .await
                    .unwrap();
                cursors.push(cursor);
            }
            assert!(cursors.len() >= 4);
            let fence = fold.lock().unwrap().begin_capture("default", "p");
            feed(&frame(1_000_000, 4));
            feed(&frame(2_000_000, 4));
            let after = repository.capture(&scope, true).await.unwrap().unwrap();
            assert_eq!(after.epoch, before.epoch);
            assert_eq!(after.trimmed, before.trimmed + 8);
            assert_eq!(after.len, before.len);
            assert_eq!(fold.lock().unwrap().begin_capture("default", "p"), fence);
            // Front trims keep still-held pages alive, not the oldest page
            // whose first row was dropped. Gone removes the whole snapshot.
            read_page(&repository, scope.clone(), Some(&cursors[0]))
                .await
                .unwrap();
            assert_eq!(
                read_page(
                    &repository,
                    scope.clone(),
                    cursors.last().map(String::as_str)
                )
                .await,
                Err(HistoryError::Gone)
            );
            assert_eq!(
                read_page(&repository, scope.clone(), Some(&cursors[0])).await,
                Err(HistoryError::Gone)
            );
            let fresh = read_page(&repository, scope.clone(), None).await.unwrap();
            assert_ne!(fresh["snapshot_id"], first["snapshot_id"]);
            // A cursor remains pinned, but a new traversal must not reuse
            // its same-epoch snapshot after append/trim has moved the window.
            feed(&frame(2_000_001, 4));
            read_page(&repository, scope.clone(), fresh["next_before"].as_str())
                .await
                .unwrap();
            let current = read_page(&repository, scope.clone(), None).await.unwrap();
            assert_ne!(current["snapshot_id"], fresh["snapshot_id"]);
            assert_eq!(
                read_page(&repository, scope.clone(), None).await.unwrap(),
                current
            );
            let after = repository.capture(&scope, true).await.unwrap().unwrap();
            assert_eq!(after.epoch, before.epoch);
            assert_eq!(after.trimmed, before.trimmed + 9);
            assert_eq!(
                current["rows"].as_array().unwrap().last().unwrap(),
                after.rows.last().unwrap()
            );
            let old_cursor = current["next_before"].as_str().unwrap().to_owned();
            let mut page = current;
            let mut tiled = Vec::new();
            loop {
                let mut rows: Vec<String> = serde_json::from_value(page["rows"].clone()).unwrap();
                rows.extend(tiled);
                tiled = rows;
                let Some(cursor) = page["next_before"].as_str() else {
                    break;
                };
                page = read_page(&repository, scope.clone(), Some(cursor))
                    .await
                    .unwrap();
            }
            assert_eq!(tiled, after.rows);
            persisted_scope(&repository, &scope).await;
            repository.close().await;

            let restarted = captures(Some("A"), false);
            let repository = fixture.open(restarted.clone()).await;
            assert_eq!(
                read_page(&repository, scope.clone(), Some(&old_cursor)).await,
                Err(HistoryError::Gone)
            );
            let checkpoint = read_page(&repository, scope.clone(), None).await.unwrap();
            let recovered = repository.capture(&scope, true).await.unwrap().unwrap();
            assert_eq!(recovered.rows, after.rows);
            assert_eq!(recovered.trimmed, 0);
            assert_eq!(recovered.len, after.len);
            assert_ne!(recovered.epoch, after.epoch);
            let metadata = repository.capture(&scope, false).await.unwrap().unwrap();
            assert_eq!(metadata.len, after.len);
            assert_eq!(metadata.trimmed, 0);
            assert!(metadata.rows.is_empty());
            assert_eq!(restarted.lock().unwrap().depth("default", "p"), 0);
            restarted.lock().unwrap().record_frame(
                "default",
                "p",
                "recent_unwrapped",
                format,
                &frame(3_000_000, 4),
            );
            assert_eq!(
                read_page(&repository, scope.clone(), None).await.unwrap(),
                checkpoint
            );
            restarted.lock().unwrap().record_frame(
                "default",
                "p",
                "recent_unwrapped",
                format,
                &frame(3_000_001, 4),
            );
            assert_eq!(
                read_page(
                    &repository,
                    scope.clone(),
                    checkpoint["next_before"].as_str()
                )
                .await,
                Err(HistoryError::Gone)
            );
            let replacement = repository.capture(&scope, true).await.unwrap().unwrap();
            assert_eq!(replacement.rows, vec![frame(3_000_000, 1)]);
            assert_ne!(replacement.epoch, recovered.epoch);
            persisted_scope(&repository, &scope).await;
            repository.close().await;
            let repository = fixture.open(captures(Some("A"), false)).await;
            assert_eq!(
                read_page(&repository, scope.clone(), None).await.unwrap()["rows"],
                json!(replacement.rows)
            );
            repository.close().await;
        }
    }
}

#[test]
fn sqlite_encoding_cap_counts_row_text_separately_and_rejects_excess() {
    let rows = vec!["x".repeat(1024); MAX_PANE_BYTES / 1024];
    let encoded = encode_rows(&rows);
    assert!(encoded.len() > MAX_PANE_BYTES);
    assert_eq!(decode_rows(&encoded).unwrap(), rows);
    let mut excess = rows;
    excess.push("x".into());
    assert!(decode_rows(&encode_rows(&excess)).is_err());
    assert!(decode_rows(&encode_rows(&vec![String::new(); MAX_PANE_LINES + 1])).is_err());
    assert!(decode_rows(&vec![0; MAX_ENCODED_BYTES + 1]).is_err());
}

#[tokio::test]
async fn unknown_to_verified_identity_quarantines_rows_and_inflight_fences_before_sqlite_recovery()
{
    for transitions in [
        vec![None, Some("B")],
        vec![Some("A"), None, Some("A")],
        vec![Some("A"), None, Some("B")],
    ] {
        let fixture = Fixture::new();
        let fold = captures(transitions[0], true);
        feed_ansi(&mut fold.lock().unwrap());
        let worker = fixture.checkpoints(fold.clone()).await;
        passes(&worker, now_seconds()).await;
        // An unverified process's projection could never be recovered, so
        // it is never written.
        let verified = if transitions[0].is_some() { 2 } else { 0 };
        assert_eq!(record_count(&worker).await, verified);
        for (index, identity) in transitions.iter().enumerate().skip(1) {
            {
                let mut store = fold.lock().unwrap();
                let old_fence = store.begin_capture("default", "p");
                assert!(old_fence.is_some());
                store.observe_native_listing("default", &[pane(*identity)]);
                assert_ne!(store.begin_capture("default", "p"), old_fence);
                let raw =
                    "unverified late 0\nunverified late 1\nunverified late 2\nunverified late 3";
                assert_eq!(
                    store.serve_read_fenced(
                        "default",
                        "p",
                        ("recent_unwrapped", "text"),
                        raw,
                        200,
                        old_fence
                    ),
                    raw
                );
                store.record_frame_fenced(
                    "default",
                    "p",
                    "recent_unwrapped",
                    "ansi",
                    raw,
                    old_fence,
                );
                for format in ["text", "ansi"] {
                    let mut scope = scope();
                    scope.format = format.into();
                    assert!(store.live_captured_history(&scope, true).unwrap().is_none());
                }
            }
            passes(&worker, now_seconds()).await;
            assert_eq!(record_count(&worker).await, 0);
            if index + 1 < transitions.len() {
                // Capture another prefix during the unknown interval; it too
                // must be discarded when the same or another process verifies.
                {
                    let mut store = fold.lock().unwrap();
                    store.record_frame(
                        "default",
                        "p",
                        "recent_unwrapped",
                        "text",
                        "unknown 0\nunknown 1\nunknown 2\nunknown 3",
                    );
                    store.record_frame(
                        "default",
                        "p",
                        "recent_unwrapped",
                        "text",
                        "unknown 1\nunknown 2\nunknown 3\nunknown 4",
                    );
                }
                passes(&worker, now_seconds()).await;
                assert_eq!(record_count(&worker).await, 0);
            }
        }
        drop(worker);
        let restarted = captures(*transitions.last().unwrap(), false);
        let worker = fixture.checkpoints(restarted.clone()).await;
        let stamp = restarted
            .lock()
            .unwrap()
            .projection_stamp(&scope())
            .unwrap();
        assert!(worker.read(stamp).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn sqlite_retention_and_quota_eviction_do_not_resurrect_unchanged_polled_projections() {
    for expire in [true, false] {
        let fixture = Fixture::new();
        let fold = captures(Some("A"), true);
        feed_ansi(&mut fold.lock().unwrap());
        let worker = fixture.checkpoints(fold.clone()).await;
        let base = now_seconds();
        passes(&worker, base).await;
        let mut ansi = scope();
        ansi.format = "ansi".into();
        let scopes = [scope(), ansi];
        let original: Vec<_> = scopes
            .iter()
            .map(|scope| {
                fold.lock()
                    .unwrap()
                    .live_captured_history(scope, true)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        let now = if expire {
            let expired = base + RETENTION_SECONDS + 1;
            worker.db.call(move |c| prune(c, expired)).await.unwrap();
            expired
        } else {
            worker
                .db
                .call(move |c| {
                    for n in 0..MAX_RECORDS {
                        let encoded = encode_rows(&["quota fixture"]);
                        c.execute(
                            "INSERT INTO history_captures VALUES (?1,'quota',?2,?3,?4)",
                            params![
                                format!("quota-{n}"),
                                encoded,
                                encoded.len() as i64,
                                base + 1
                            ],
                        )?;
                    }
                    prune(c, base + 1)
                })
                .await
                .unwrap();
            base + 2
        };
        for scope in &scopes {
            let stamp = fold.lock().unwrap().projection_stamp(scope).unwrap();
            assert!(worker.read(stamp).await.unwrap().is_none());
        }
        for _ in 0..3 {
            {
                let mut store = fold.lock().unwrap();
                store.record_frame(
                    "default",
                    "p",
                    "recent_unwrapped",
                    "text",
                    "row 7\nrow 8\nrow 9\nrow 10",
                );
                store.record_frame("default", "p", "recent_unwrapped", "ansi", &ansi_frame(7));
            }
            passes(&worker, now).await;
            for (scope, original) in scopes.iter().zip(&original) {
                let stamp = {
                    let store = fold.lock().unwrap();
                    let capture = store.live_captured_history(scope, true).unwrap().unwrap();
                    assert_eq!(capture.rows, original.rows);
                    assert_eq!(capture.epoch, original.epoch);
                    store.projection_stamp(scope).unwrap()
                };
                assert!(
                    worker.read(stamp).await.unwrap().is_none(),
                    "unchanged poll resurrected an evicted projection"
                );
            }
        }
        fold.lock().unwrap().record_frame(
            "default",
            "p",
            "recent_unwrapped",
            "text",
            "row 8\nrow 9\nrow 10\nrow 11",
        );
        passes(&worker, now + 1).await;
        let text_stamp = fold.lock().unwrap().projection_stamp(&scopes[0]).unwrap();
        let (rows, _) = worker.read(text_stamp.clone()).await.unwrap().unwrap();
        let live = fold
            .lock()
            .unwrap()
            .live_captured_history(&scopes[0], true)
            .unwrap()
            .unwrap()
            .rows;
        assert_eq!(rows, live);
        assert_ne!(rows, original[0].rows);
        let text_updated = updated(&worker, worker.key(&text_stamp).unwrap()).await;
        assert_eq!(text_updated, now + 1);
        let ansi_stamp = fold.lock().unwrap().projection_stamp(&scopes[1]).unwrap();
        assert!(
            worker.read(ansi_stamp).await.unwrap().is_none(),
            "text change renewed ANSI retention"
        );
        fold.lock().unwrap().record_frame(
            "default",
            "p",
            "recent_unwrapped",
            "ansi",
            &ansi_frame(8),
        );
        passes(&worker, now + 2).await;
        let ansi_stamp = fold.lock().unwrap().projection_stamp(&scopes[1]).unwrap();
        assert!(worker.read(ansi_stamp.clone()).await.unwrap().is_some());
        assert_eq!(
            updated(&worker, worker.key(&ansi_stamp).unwrap()).await,
            now + 2
        );
        assert_eq!(
            updated(&worker, worker.key(&text_stamp).unwrap()).await,
            text_updated
        );
    }
}

async fn persisted(repository: &SqliteHistoryRepository) {
    persisted_scope(repository, &scope()).await;
}

/// Wait for the write loop to store the fold's current projection.
async fn persisted_scope(repository: &SqliteHistoryRepository, scope: &HistoryScope) {
    let checkpoints = repository.checkpoints.as_ref().unwrap();
    let (stamp, expected) = {
        let store = checkpoints.captures.lock().unwrap();
        (
            store.projection_stamp(scope).unwrap(),
            store
                .live_captured_history(scope, true)
                .unwrap()
                .unwrap()
                .rows,
        )
    };
    for _ in 0..100 {
        if checkpoints
            .read(stamp.clone())
            .await
            .unwrap()
            .is_some_and(|(rows, _)| rows == expected)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("projection was not persisted");
}

#[tokio::test]
async fn real_worker_close_reopen_recovers_checkpoint_not_viewport_and_old_cursor_is_gone() {
    let fixture = Fixture::new();
    let repository = fixture.open(captures(Some("verified-root-A"), true)).await;
    persisted(&repository).await;
    let (first, concurrent) = tokio::join!(
        read_page(&repository, scope(), None),
        read_page(&repository, scope(), None)
    );
    let first = first.unwrap();
    assert_eq!(first, concurrent.unwrap());
    let cursor = first["next_before"].as_str().unwrap().to_owned();
    repository.close().await;
    let fold = captures(Some("verified-root-A"), false);
    let repository = fixture.open(fold.clone()).await;
    let mut restarted = scope();
    restarted.generation = "new-run".into();
    assert_eq!(
        read_page(&repository, restarted.clone(), Some(&cursor)).await,
        Err(HistoryError::Gone)
    );
    let checkpoint = read_page(&repository, restarted.clone(), None)
        .await
        .unwrap();
    assert_eq!(checkpoint["rows"], json!(["row 5", "row 6"]));
    assert_ne!(checkpoint["capture_epoch"], first["capture_epoch"]);
    assert_eq!(fold.lock().unwrap().depth("default", "p"), 0);
    fold.lock().unwrap().record_frame(
        "default",
        "p",
        "recent_unwrapped",
        "text",
        "new 0\nnew 1\nnew 2\nnew 3",
    );
    assert_eq!(
        read_page(&repository, restarted.clone(), None)
            .await
            .unwrap(),
        checkpoint
    );
    fold.lock().unwrap().record_frame(
        "default",
        "p",
        "recent_unwrapped",
        "text",
        "new 1\nnew 2\nnew 3\nnew 4",
    );
    assert_eq!(
        read_page(&repository, restarted, None).await.unwrap()["rows"],
        json!(["new 0"])
    );
    persisted(&repository).await;
    repository.close().await;
    let repository = fixture.open(captures(Some("verified-root-A"), false)).await;
    assert_eq!(
        read_page(&repository, scope(), None).await.unwrap()["rows"],
        json!(["new 0"])
    );
    repository.close().await;
}

#[tokio::test]
async fn cross_process_unknown_install_endpoint_and_config_membership_fail_closed() {
    let mut fixture = Fixture::new();
    let repository = fixture.open(captures(Some("A"), true)).await;
    persisted(&repository).await;
    repository.close().await;
    for identity in [None, Some("B")] {
        let repository = fixture.open(captures(identity, false)).await;
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["availability"],
            "not_captured"
        );
        repository.close().await;
    }
    let initial = fixture.config.clone();
    for change in 0..3 {
        fixture.config = initial.clone();
        match change {
            0 => fixture.config.server_id = "other-install".into(),
            1 => fixture.config.sessions[0].socket_path.push_str(".changed"),
            _ => fixture.config.sessions.clear(),
        }
        let repository = fixture.open(captures(Some("A"), false)).await;
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["availability"],
            "not_captured"
        );
        repository.close().await;
        fixture.config = initial.clone();
        let repository = fixture.open(captures(Some("A"), false)).await;
        assert_eq!(
            read_page(&repository, scope(), None).await.unwrap()["availability"],
            "not_captured"
        );
        repository.close().await;
    }
}

#[test]
fn checkpoint_read_fence_rejects_delayed_recovery_after_reset_but_not_a_screen_frame() {
    for reset in 0..3 {
        let store = captures(Some("A"), false);
        let mut store = store.lock().unwrap();
        let stamp = store.projection_stamp(&scope()).unwrap();
        match reset {
            0 => {
                let mut p = pane(Some("A"));
                p.width = Some(120);
                store.observe_native_listing("default", &[p]);
            }
            1 => store.observe_native_listing("default", &[pane(Some("B"))]),
            _ => store.observe_listing("default", &json!({"panes": []})),
        }
        assert!(!store.recover_checkpoint(
            &stamp,
            vec!["old".into()],
            Instant::now() + Duration::from_secs(3600)
        ));
    }
    let store = captures(Some("A"), false);
    let mut store = store.lock().unwrap();
    let stamp = store.projection_stamp(&scope()).unwrap();
    // A frame streamed during the checkpoint read moves the revision but
    // shows only a mutable screen: recovery still succeeds.
    store.record_frame("default", "p", "recent_unwrapped", "text", "screen");
    assert!(!store.recover_checkpoint(&stamp, vec!["expired".into()], Instant::now()));
    assert!(store.recover_checkpoint(
        &stamp,
        vec!["checkpoint".into()],
        Instant::now() + Duration::from_secs(3600)
    ));
    assert_ne!(
        store.projection_stamp(&scope()).unwrap().revision,
        stamp.revision
    );
    assert!(store
        .live_captured_history(&scope(), true)
        .unwrap()
        .is_some_and(|live| live.rows.is_empty()));
    assert_eq!(
        store
            .captured_history(&scope(), true)
            .unwrap()
            .unwrap()
            .rows,
        vec!["checkpoint".to_owned()]
    );
}

#[tokio::test]
async fn sqlite_formats_are_independent_and_destructive_shrink_removes_durable_projection() {
    let fixture = Fixture::new();
    let fold = captures(Some("A"), true);
    feed_ansi(&mut fold.lock().unwrap());
    let repository = fixture.open(fold).await;
    let mut ansi = scope();
    ansi.format = "ansi".into();
    persisted_scope(&repository, &ansi).await;
    persisted(&repository).await;
    repository.close().await;
    let fold = captures(Some("A"), false);
    let repository = fixture.open(fold.clone()).await;
    assert_eq!(
        read_page(&repository, ansi.clone(), None).await.unwrap()["rows"],
        json!(["\u{1b}[31mrow 5\u{1b}[0m", "\u{1b}[31mrow 6\u{1b}[0m"])
    );
    let first = read_page(&repository, scope(), None).await.unwrap();
    assert_eq!(first["rows"], json!(["row 5", "row 6"]));
    {
        let mut store = fold.lock().unwrap();
        let mut editor = pane(Some("A"));
        editor.foreground_command = Some("nvim".into());
        store.observe_native_listing("default", &[editor]);
        store.record_frame("default", "p", "recent_unwrapped", "text", "editor only");
    }
    assert_eq!(
        read_page(&repository, scope(), first["next_before"].as_str()).await,
        Err(HistoryError::Gone)
    );
    assert_eq!(
        read_page(&repository, scope(), None).await.unwrap()["rows"],
        json!([])
    );
    // Give both dirty shapes a pass on the store worker.
    let checkpoints = repository.checkpoints.clone().unwrap();
    passes(&checkpoints, now_seconds()).await;
    drop(checkpoints);
    repository.close().await;
    let db = fixture.db();
    assert_eq!(
        sql_i64(&db, "SELECT count(*) FROM history_captures").await,
        0
    );
}

#[tokio::test]
async fn storage_failures_spare_memory_and_heal_on_the_next_working_pass() {
    // A store without the history namespace cannot initialize: what memory
    // holds is still served; only a recovery that needs the store is a 503.
    let fixture = Fixture::new();
    let db = SqliteStore::open(&fixture.db_path(), None, &[]).unwrap();
    let repository =
        SqliteHistoryRepository::open(db, &fixture.config, captures(Some("A"), true)).await;
    assert_eq!(
        read_page(&repository, scope(), None).await.unwrap()["rows"],
        json!(["row 5", "row 6"])
    );
    repository.close().await;
    let db = SqliteStore::open(&fixture.db_path(), None, &[]).unwrap();
    let repository =
        SqliteHistoryRepository::open(db, &fixture.config, captures(Some("A"), false)).await;
    assert_eq!(
        read_page(&repository, scope(), None).await,
        Err(HistoryError::Unavailable)
    );
    repository.close().await;

    // A write that fails marks the store unhealthy; the traversal pinned in
    // memory goes on, and the loop retries until a pass works again.
    let fixture = Fixture::new();
    let fold = captures(Some("A"), true);
    let repository = fixture.open(fold.clone()).await;
    persisted(&repository).await;
    let first = read_page(&repository, scope(), None).await.unwrap();
    let checkpoints = repository.checkpoints.clone().unwrap();
    checkpoints
        .db
        .call(|c| Ok(c.execute_batch("DROP TABLE history_captures")?))
        .await
        .unwrap();
    fold.lock().unwrap().record_frame(
        "default",
        "p",
        "recent_unwrapped",
        "text",
        "row 8\nrow 9\nrow 10\nrow 11",
    );
    for _ in 0..100 {
        if !checkpoints.healthy.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!checkpoints.healthy.load(Ordering::Acquire));
    assert_eq!(
        read_page(&repository, scope(), first["next_before"].as_str())
            .await
            .unwrap()["rows"],
        json!(["row 3", "row 4"])
    );
    checkpoints
        .db
        .call(|c| Ok(c.execute_batch("CREATE TABLE history_captures(identity TEXT PRIMARY KEY, policy TEXT NOT NULL, rows BLOB NOT NULL, bytes INTEGER NOT NULL, updated INTEGER NOT NULL); CREATE INDEX history_capture_quota ON history_captures(updated, identity, bytes);")?))
        .await
        .unwrap();
    for _ in 0..400 {
        if checkpoints.healthy.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(checkpoints.healthy.load(Ordering::Acquire), "healed");
    drop(checkpoints);
    repository.close().await;
}

#[tokio::test]
async fn queued_read_overload_is_not_memory_success() {
    let fixture = Fixture::new();
    let repository = fixture.open(captures(Some("A"), true)).await;
    persisted(&repository).await;
    repository.close().await;
    // An empty fold with a verified identity must consult the store.
    let repository = fixture.open(captures(Some("A"), false)).await;
    let held = repository
        .reads
        .try_acquire_many(MAX_PENDING_READS as u32)
        .unwrap();
    assert_eq!(
        read_page(&repository, scope(), None).await,
        Err(HistoryError::Unavailable)
    );
    drop(held);
    assert_eq!(
        read_page(&repository, scope(), None).await.unwrap()["rows"],
        json!(["row 5", "row 6"])
    );
    repository.close().await;
}

#[tokio::test]
async fn retention_and_logical_quotas_keep_the_newest_updates_and_reads_do_not_renew() {
    // (declared bytes per fixture row, fixtures that survive besides the
    // newest one): the record quota binds first, then the payload quota.
    for (bytes, older_survivors) in [(500_004_i64, 46_usize), (1_000_000, 24)] {
        let fixture = Fixture::new();
        let worker = fixture.checkpoints(captures(Some("A"), true)).await;
        passes(&worker, now_seconds()).await;
        let aged = now_seconds() - 1000;
        worker
            .db
            .call(move |c| Ok(c.execute("UPDATE history_captures SET updated=?1", [aged])?))
            .await
            .unwrap();
        let stamp = worker
            .captures
            .lock()
            .unwrap()
            .projection_stamp(&scope())
            .unwrap();
        let real = worker.key(&stamp).unwrap();
        assert!(worker.read(stamp.clone()).await.unwrap().is_some());
        assert_eq!(updated(&worker, real.clone()).await, aged);
        // fixture-0..58 are older than the real capture, one second apart;
        // fixture-59 is newer than it.
        worker
            .db
            .call(move |c| {
                for n in 0..60_i64 {
                    let at = if n == 59 { aged + 1 } else { aged - 59 + n };
                    c.execute(
                        "INSERT INTO history_captures VALUES (?1,'policy',?2,?3,?4)",
                        params![format!("fixture-{n:02}"), encode_rows(&["x"]), bytes, at],
                    )?;
                }
                prune(c, now_seconds())
            })
            .await
            .unwrap();
        let survivors: Vec<String> = worker
            .db
            .call(|c| {
                Ok(
                    c.prepare("SELECT identity FROM history_captures ORDER BY updated DESC")?
                        .query_map([], |row| row.get(0))?
                        .collect::<Result<_, _>>()?,
                )
            })
            .await
            .unwrap();
        let mut expected = vec!["fixture-59".to_owned(), real.clone()];
        expected.extend(
            (59 - older_survivors..59)
                .rev()
                .map(|n| format!("fixture-{n:02}")),
        );
        assert_eq!(survivors, expected, "{bytes}");
        let (count, total): (i64, i64) = worker
            .db
            .call(|c| {
                Ok(c.query_row(
                    "SELECT count(*),sum(bytes) FROM history_captures",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert!(count <= MAX_RECORDS && total <= MAX_PAYLOAD_BYTES);
        // Reading the survivor did not renew it.
        assert!(worker.read(stamp.clone()).await.unwrap().is_some());
        assert_eq!(updated(&worker, real).await, aged);
        let expired = now_seconds() - RETENTION_SECONDS - 1;
        worker
            .db
            .call(move |c| {
                c.execute("UPDATE history_captures SET updated=?1", [expired])?;
                prune(c, now_seconds())
            })
            .await
            .unwrap();
        assert_eq!(record_count(&worker).await, 0);
    }
}

/// Two changes in quick succession are one write: the second waits out the
/// debounce in the fold, and only the latest state reaches the store.
#[tokio::test]
async fn quick_changes_to_one_scope_are_written_once() {
    let fixture = Fixture::new();
    let fold = captures(Some("A"), true);
    let worker = fixture
        .checkpoints_with(fold.clone(), Duration::from_secs(3600))
        .await;
    passes(&worker, now_seconds()).await;
    let stamp = fold.lock().unwrap().projection_stamp(&scope()).unwrap();
    let first = worker.read(stamp).await.unwrap().unwrap().0;
    for top in [8, 9] {
        fold.lock().unwrap().record_frame(
            "default",
            "p",
            "recent_unwrapped",
            "text",
            &(top..top + 4)
                .map(|n| format!("row {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        // Deferred, so the pass is not idle and the loop keeps coming back.
        assert!(!worker.flush_at(now_seconds()).await.unwrap());
        let stamp = fold.lock().unwrap().projection_stamp(&scope()).unwrap();
        assert_eq!(worker.read(stamp).await.unwrap().unwrap().0, first);
    }
    // The debounce runs out: one write carries the latest projection.
    for seen in worker.writer.lock().unwrap().seen.values_mut() {
        seen.written -= Duration::from_secs(3600);
    }
    assert!(!worker.flush_at(now_seconds()).await.unwrap());
    let (stamp, live) = {
        let store = fold.lock().unwrap();
        (
            store.projection_stamp(&scope()).unwrap(),
            store
                .live_captured_history(&scope(), true)
                .unwrap()
                .unwrap()
                .rows,
        )
    };
    assert_eq!(worker.read(stamp).await.unwrap().unwrap().0, live);
    assert_ne!(live, first);
    assert!(worker.flush_at(now_seconds()).await.unwrap());
    // A reset is written at once, debounce or not.
    fold.lock()
        .unwrap()
        .observe_native_listing("default", &[pane(Some("B"))]);
    passes(&worker, now_seconds()).await;
    assert_eq!(record_count(&worker).await, 0);
}

/// The write loop skips the store worker only while a pass reports idle.
#[tokio::test]
async fn a_pass_reports_idle_only_when_every_projection_is_written() {
    let fixture = Fixture::new();
    let fold = captures(Some("A"), true);
    let worker = fixture.checkpoints(fold.clone()).await;
    assert!(!worker.flush_at(now_seconds()).await.unwrap());
    assert!(worker.flush_at(now_seconds()).await.unwrap());
    fold.lock().unwrap().record_frame(
        "default",
        "p",
        "recent_unwrapped",
        "text",
        "row 8\nrow 9\nrow 10\nrow 11",
    );
    assert!(!worker.flush_at(now_seconds()).await.unwrap());
    assert!(worker.flush_at(now_seconds()).await.unwrap());
}

#[test]
fn rows_round_trip_and_malformed_encodings_are_rejected() {
    for rows in [
        vec!["\u{1b}[31mansi\u{1b}[0m".into(), "".into(), "text".into()],
        vec!["x".repeat(super::super::history::MAX_PAGE_BYTES - 1)],
    ] {
        assert_eq!(decode_rows(&encode_rows(&rows)).unwrap(), rows);
    }
    for bad in [vec![1], vec![10, 0, 0, 0], vec![1, 0, 0, 0, 255]] {
        assert!(decode_rows(&bad).is_err());
    }
}

#[tokio::test]
async fn default_memory_does_not_touch_the_history_tables() {
    let fixture = Fixture::new();
    let mut config = fixture.config.clone();
    config.history.storage = HistoryStorage::Memory;
    let db = fixture.db();
    let repository = repository(&config, db.clone(), captures(Some("A"), true)).await;
    assert_eq!(
        read_page(repository.as_ref(), scope(), None).await.unwrap()["rows"],
        json!(["row 5", "row 6"])
    );
    // Long enough for a write loop, were there one, to have run.
    tokio::time::sleep(FLUSH_TICK * 3).await;
    assert_eq!(
        sql_i64(&db, "SELECT count(*) FROM history_backends").await,
        0
    );
    assert_eq!(
        sql_i64(&db, "SELECT count(*) FROM history_captures").await,
        0
    );
}

#[tokio::test]
async fn an_idle_fold_does_not_wake_the_writer() {
    let fixture = Fixture::new();
    let fold = captures(Some("A"), true);
    let repository = fixture.open(fold.clone()).await;
    persisted(&repository).await;
    let checkpoints = repository.checkpoints.clone().unwrap();
    let count = || checkpoints.wakes.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let settled = count();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(count(), settled, "no wake while nothing changes");
    fold.lock().unwrap().record_frame(
        "default",
        "p",
        "recent_unwrapped",
        "text",
        "row 8\nrow 9\nrow 10\nrow 11",
    );
    for _ in 0..100 {
        if count() > settled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(count() > settled, "a change wakes it");
    drop(checkpoints);
    repository.close().await;
}

#[tokio::test]
async fn history_writes_skip_the_fsync_and_an_idle_prune_folds_the_wal_back() {
    let fixture = Fixture::new();
    let checkpoints = fixture.checkpoints(captures(Some("A"), true)).await;
    passes(&checkpoints, now_seconds()).await;
    assert_eq!(record_count(&checkpoints).await, 1);
    let wal = || checkpoints.db.call(|c| Ok(wal_bytes(c)));
    assert!(wal().await.unwrap() > 0, "the write went to the WAL");
    // The connection the other domains share keeps FULL durability.
    assert_eq!(sql_i64(&checkpoints.db, "PRAGMA synchronous").await, 2);
    let stamps = checkpoints.captures.lock().unwrap().projection_stamps();
    checkpoints
        .pass(now_seconds(), true, stamps, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(wal().await.unwrap(), 0, "an idle prune truncates the WAL");
    assert_eq!(record_count(&checkpoints).await, 1, "nothing was lost");
}
