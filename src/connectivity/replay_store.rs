//! Durable request identities and monotonic transport versions, scoped to the
//! current credential. Legacy envelopes cannot prove their timestamp: retain
//! their accepted nonces until the credential upgrades, never age them out.
use std::sync::Arc;

use rusqlite::{params, OptionalExtension as _};

use crate::platform::store::sqlite::{Migration, SqliteStore};

use super::transport::{timestamp_is_fresh, MAX_CLOCK_SKEW_MS, SUPPORTED_VERSIONS, VERSION_V2};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "transport",
    steps: &[
        "
CREATE TABLE transport_credentials (
  device_id TEXT NOT NULL, credential_hash TEXT NOT NULL,
  minimum_version INTEGER NOT NULL,
  PRIMARY KEY(device_id, credential_hash));
CREATE TABLE transport_nonces (
  device_id TEXT NOT NULL, credential_hash TEXT NOT NULL, nonce BLOB NOT NULL,
  version INTEGER NOT NULL, timestamp_ms INTEGER NOT NULL,
  PRIMARY KEY(device_id, credential_hash, nonce));
CREATE INDEX transport_nonces_expiry ON transport_nonces(version, timestamp_ms);
",
        "
CREATE TABLE transport_pruning (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  cutoff_ms INTEGER NOT NULL CHECK(cutoff_ms>=0));
INSERT INTO transport_pruning(singleton,cutoff_ms) VALUES(1,0);
",
    ],
};

// Refuse admission rather than evicting identities that still prevent replay.
// Upgrading to v2 releases legacy history; no pairing/key rotation is needed.
pub(crate) const MAX_NONCES_PER_CREDENTIAL: i64 = 262_144;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Acceptance {
    Accepted,
    Replayed,
    VersionRequired,
    Capacity,
    InvalidMetadata,
}

pub(crate) async fn accept<F>(
    db: &Arc<SqliteStore>,
    device_id: String,
    credential_hash: String,
    nonce: [u8; 12],
    version: u8,
    timestamp_ms: u128,
    clock: F,
) -> anyhow::Result<Acceptance>
where
    F: FnOnce() -> u128 + Send + 'static,
{
    db.call(move |connection| {
        let tx = connection.transaction()?;
        // Sample on the store thread, not before waiting in its queue. No
        // admission or pruning may succeed using freshness checked at decryption.
        let now_ms = clock();
        if !SUPPORTED_VERSIONS.contains(&version) || !timestamp_is_fresh(timestamp_ms, now_ms) {
            return Ok(Acceptance::InvalidMetadata);
        }
        let (Ok(timestamp), Ok(oldest)) = (
            i64::try_from(timestamp_ms),
            i64::try_from(now_ms.saturating_sub(MAX_CLOCK_SKEW_MS)),
        ) else {
            return Ok(Acceptance::InvalidMetadata);
        };
        let cutoff: i64 = tx.query_row(
            "SELECT cutoff_ms FROM transport_pruning WHERE singleton=1", [], |r| r.get(0),
        )?;
        // Global GC can remove another credential's identity. Persist its
        // high-water mark so a clock rollback, even after restart, cannot make
        // those authenticated timestamps admissible again. Equality is fresh.
        if version == VERSION_V2 && timestamp < cutoff {
            return Ok(Acceptance::InvalidMetadata);
        }
        let oldest = oldest.max(cutoff);
        let minimum: u8 = tx.query_row(
            "SELECT minimum_version FROM transport_credentials WHERE device_id=?1 AND credential_hash=?2",
            params![device_id, credential_hash], |r| r.get(0),
        ).optional()?.unwrap_or(1);
        if version < minimum {
            return Ok(Acceptance::VersionRequired);
        }
        // v2 freshness is authenticated. v1 freshness is not, so its identities
        // remain durable even across process restart and arbitrarily long delays.
        tx.execute("DELETE FROM transport_nonces WHERE version=2 AND timestamp_ms < ?1", [oldest])?;
        tx.execute("UPDATE transport_pruning SET cutoff_ms=?1 WHERE singleton=1", [oldest])?;
        let seen: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM transport_nonces WHERE device_id=?1 AND credential_hash=?2 AND nonce=?3)",
            params![device_id, credential_hash, nonce.as_slice()], |r| r.get(0),
        )?;
        if seen { return Ok(Acceptance::Replayed); }
        if version == VERSION_V2 && minimum < VERSION_V2 {
            // Once pinned, no v1 request can pass, even if its nonce was never
            // recorded. Its historical identities can therefore be discarded.
            tx.execute("DELETE FROM transport_nonces WHERE device_id=?1 AND credential_hash=?2 AND version=1", params![device_id, credential_hash])?;
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM transport_nonces WHERE device_id=?1 AND credential_hash=?2",
            params![device_id, credential_hash], |r| r.get(0),
        )?;
        if count >= MAX_NONCES_PER_CREDENTIAL { return Ok(Acceptance::Capacity); }
        tx.execute(
            "INSERT INTO transport_nonces(device_id,credential_hash,nonce,version,timestamp_ms) VALUES(?1,?2,?3,?4,?5)",
            params![device_id, credential_hash, nonce.as_slice(), version, timestamp],
        )?;
        tx.execute(
            "INSERT INTO transport_credentials(device_id,credential_hash,minimum_version) VALUES(?1,?2,?3) ON CONFLICT(device_id,credential_hash) DO UPDATE SET minimum_version=MAX(minimum_version,excluded.minimum_version)",
            params![device_id, credential_hash, version],
        )?;
        tx.commit()?;
        Ok(Acceptance::Accepted)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> std::path::PathBuf {
        crate::test_state_dir().join(format!("replay-{}.sqlite3", uuid::Uuid::new_v4()))
    }

    async fn record(
        db: &Arc<SqliteStore>,
        nonce: [u8; 12],
        version: u8,
        timestamp: u128,
        now: u128,
    ) -> Acceptance {
        accept(
            db,
            "fixture-device".into(),
            "fixture-credential".into(),
            nonce,
            version,
            timestamp,
            move || now,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn legacy_identity_survives_clock_window_and_restart() {
        let path = path();
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 1, 1000, 1000).await,
            Acceptance::Accepted
        );
        drop(db);
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 1, 900000, 900000).await,
            Acceptance::Replayed
        );
        assert_eq!(
            record(&db, [2; 12], 1, 900000, 900000).await,
            Acceptance::Accepted
        );
    }

    #[tokio::test]
    async fn version_pin_is_durable_monotonic_and_credential_scoped() {
        let path = path();
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 1, 1000, 1000).await,
            Acceptance::Accepted
        );
        assert_eq!(
            record(&db, [2; 12], 2, 1001, 1001).await,
            Acceptance::Accepted
        );
        drop(db);
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [3; 12], 1, 1002, 1002).await,
            Acceptance::VersionRequired
        );
        assert_eq!(
            record(&db, [2; 12], 2, 1001, 1002).await,
            Acceptance::Replayed
        );
        assert_eq!(
            accept(
                &db,
                "fixture-device".into(),
                "new-fixture-credential".into(),
                [3; 12],
                1,
                1002,
                || 1002
            )
            .await
            .unwrap(),
            Acceptance::Accepted
        );
    }

    #[tokio::test]
    async fn duplicate_admission_is_atomic_and_v2_expiry_can_be_pruned() {
        let db = SqliteStore::open(&path(), None, &[MIGRATION]).unwrap();
        let (a, b) = tokio::join!(
            record(&db, [1; 12], 2, 1000, 1000),
            record(&db, [1; 12], 2, 1000, 1000)
        );
        assert!(matches!(
            (a, b),
            (Acceptance::Accepted, Acceptance::Replayed)
                | (Acceptance::Replayed, Acceptance::Accepted)
        ));
        assert_eq!(
            record(&db, [2; 12], 2, 900000, 900000).await,
            Acceptance::Accepted
        );
        let count: i64 = db
            .call(|c| Ok(c.query_row("SELECT count(*) FROM transport_nonces", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn canonical_nonce_bytes_share_the_same_durable_identity() {
        use base64::Engine as _;
        let db = SqliteStore::open(&path(), None, &[MIGRATION]).unwrap();
        let bytes = [255; 12];
        let first = super::super::transport::nonce_bytes(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        )
        .unwrap();
        let second = super::super::transport::nonce_bytes(
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        )
        .unwrap();
        assert_eq!(
            record(&db, first, 1, 1000, 1000).await,
            Acceptance::Accepted
        );
        assert_eq!(
            record(&db, second, 1, 1000, 1000).await,
            Acceptance::Replayed
        );
    }

    async fn ledger(db: &Arc<SqliteStore>) -> (i64, i64, i64) {
        db.call(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM transport_nonces", [], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM transport_credentials", [], |r| {
                    r.get(0)
                })?,
                c.query_row("SELECT cutoff_ms FROM transport_pruning", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn freshness_boundaries_are_inclusive_for_both_versions() {
        for version in SUPPORTED_VERSIONS {
            for (timestamp, expected) in [
                (700_000, Acceptance::Accepted),
                (1_300_000, Acceptance::Accepted),
                (699_999, Acceptance::InvalidMetadata),
                (1_300_001, Acceptance::InvalidMetadata),
            ] {
                let db = SqliteStore::open(&path(), None, &[MIGRATION]).unwrap();
                assert_eq!(
                    record(&db, [1; 12], version, timestamp, 1_000_000).await,
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn invalid_metadata_never_prunes_records_or_pins_credentials() {
        let db = SqliteStore::open(&path(), None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 2, 1000, 1000).await,
            Acceptance::Accepted
        );
        let before = ledger(&db).await;
        for (version, timestamp, now) in [
            (2, 1000, 900_000),
            (1, 1000, 900_000),
            (2, 1_200_001, 900_000),
            (3, 900_000, 900_000),
            (0, 900_000, 900_000),
            (2, u128::MAX, 900_000),
            (1, u128::MAX, u128::MAX),
            (2, u128::MAX, u128::MAX),
            (2, i64::MAX as u128 + 1, i64::MAX as u128 + 1),
        ] {
            assert_eq!(
                accept(
                    &db,
                    "other-device".into(),
                    "other-credential".into(),
                    [2; 12],
                    version,
                    timestamp,
                    move || now,
                )
                .await
                .unwrap(),
                Acceptance::InvalidMetadata
            );
            assert_eq!(ledger(&db).await, before);
        }
        // Neither a version pin nor an accepted nonce was created by the failures.
        assert_eq!(
            accept(
                &db,
                "other-device".into(),
                "other-credential".into(),
                [2; 12],
                1,
                900_000,
                || 900_000,
            )
            .await
            .unwrap(),
            Acceptance::Accepted
        );
    }

    #[tokio::test]
    async fn queued_admission_rechecks_clock_after_open_at_the_boundary() {
        use super::super::transport::{self, Direction};
        use std::sync::atomic::{AtomicU64, Ordering};

        let db = SqliteStore::open(&path(), None, &[MIGRATION]).unwrap();
        let timestamp = 1_000_000;
        let boundary = timestamp + MAX_CLOCK_SKEW_MS;
        let clock = Arc::new(AtomicU64::new(boundary as u64));
        let key = [7; 32];
        let envelope =
            transport::seal_version(2, &key, Direction::Request, b"aad", b"body", timestamp)
                .unwrap();
        let nonce = transport::nonce_bytes(&envelope.nonce).unwrap();
        // Exercise both a first admission and a replay of an identity which GC
        // would otherwise remove immediately before its lookup.
        for already_seen in [false, true] {
            clock.store(boundary as u64, Ordering::SeqCst);
            if already_seen {
                assert_eq!(
                    record(&db, nonce, 2, timestamp, timestamp).await,
                    Acceptance::Accepted
                );
            }
            let before = ledger(&db).await;
            assert_eq!(
                transport::open(&key, Direction::Request, b"aad", &envelope, boundary).unwrap(),
                b"body"
            );

            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker_db = db.clone();
            let blocker = tokio::spawn(async move {
                worker_db
                    .call(move |_| {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap();
            });
            entered_rx.await.unwrap();
            let admission_clock = clock.clone();
            let admission = accept(
                &db,
                "fixture-device".into(),
                "fixture-credential".into(),
                nonce,
                2,
                timestamp,
                move || u128::from(admission_clock.load(Ordering::SeqCst)),
            );
            tokio::pin!(admission);
            // Poll once to place the admission job behind the blocked DB job.
            tokio::select! {
                biased;
                result = &mut admission => panic!("admission bypassed barrier: {result:?}"),
                _ = std::future::ready(()) => {},
            }
            clock.store((boundary + 1) as u64, Ordering::SeqCst);
            release_tx.send(()).unwrap();
            assert_eq!(admission.await.unwrap(), Acceptance::InvalidMetadata);
            blocker.await.unwrap();
            assert_eq!(ledger(&db).await, before);
        }
    }

    #[tokio::test]
    async fn global_pruning_cutoff_survives_clock_rollback_and_restart() {
        let path = path();
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 2, 1000, 1000).await,
            Acceptance::Accepted
        );
        assert_eq!(
            accept(
                &db,
                "legacy-device".into(),
                "legacy-credential".into(),
                [2; 12],
                1,
                1000,
                || 1000,
            )
            .await
            .unwrap(),
            Acceptance::Accepted
        );
        // Another credential's accepted request globally prunes v2, not v1.
        assert_eq!(
            accept(
                &db,
                "other-device".into(),
                "other-credential".into(),
                [3; 12],
                1,
                900_000,
                || 900_000,
            )
            .await
            .unwrap(),
            Acceptance::Accepted
        );
        assert_eq!(ledger(&db).await, (2, 3, 600_000));
        drop(db);
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        let before = ledger(&db).await;
        for nonce in [[1; 12], [4; 12]] {
            assert_eq!(
                record(&db, nonce, 2, 1000, 1000).await,
                Acceptance::InvalidMetadata
            );
            assert_eq!(ledger(&db).await, before);
        }
        assert_eq!(
            accept(
                &db,
                "legacy-device".into(),
                "legacy-credential".into(),
                [2; 12],
                1,
                1000,
                || 1000,
            )
            .await
            .unwrap(),
            Acceptance::Replayed
        );
        // The original v2 pin is also still durable after its nonce was pruned.
        assert_eq!(
            record(&db, [5; 12], 1, 1000, 1000).await,
            Acceptance::VersionRequired
        );
        // A rolled-back clock must not lower the global cutoff on acceptance.
        assert_eq!(
            accept(
                &db,
                "other-device".into(),
                "other-credential".into(),
                [6; 12],
                1,
                1000,
                || 1000,
            )
            .await
            .unwrap(),
            Acceptance::Accepted
        );
        assert_eq!(ledger(&db).await.2, 600_000);
        assert_eq!(
            record(&db, [7; 12], 2, 599_999, 600_000).await,
            Acceptance::InvalidMetadata
        );
        assert_eq!(
            record(&db, [7; 12], 2, 600_000, 600_000).await,
            Acceptance::Accepted
        );
        assert_eq!(ledger(&db).await.2, 600_000);
    }

    #[tokio::test]
    async fn pruning_retains_the_exact_expiry_boundary() {
        let db = SqliteStore::open(&path(), None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 2, 700_000, 700_000).await,
            Acceptance::Accepted
        );
        assert_eq!(
            record(&db, [2; 12], 2, 1_000_000, 1_000_000).await,
            Acceptance::Accepted
        );
        assert_eq!(ledger(&db).await.0, 2);
        assert_eq!(
            record(&db, [1; 12], 2, 700_000, 1_000_000).await,
            Acceptance::Replayed
        );
        assert_eq!(
            record(&db, [3; 12], 2, 1_000_001, 1_000_001).await,
            Acceptance::Accepted
        );
        assert_eq!(ledger(&db).await.0, 2);
    }

    #[tokio::test]
    async fn appended_migration_preserves_existing_history_and_pins() {
        let path = path();
        let db = SqliteStore::open(
            &path,
            None,
            &[Migration {
                namespace: "transport",
                steps: &MIGRATION.steps[..1],
            }],
        )
        .unwrap();
        db.call(|c| {
            c.execute("INSERT INTO transport_credentials VALUES('fixture-device','fixture-credential',2)", [])?;
            c.execute("INSERT INTO transport_nonces VALUES('fixture-device','fixture-credential',?1,2,1000)", [[1u8; 12].as_slice()])?;
            Ok(())
        }).await.unwrap();
        drop(db);
        let db = SqliteStore::open(&path, None, &[MIGRATION]).unwrap();
        assert_eq!(
            record(&db, [1; 12], 2, 1000, 1000).await,
            Acceptance::Replayed
        );
        assert_eq!(
            record(&db, [2; 12], 1, 1000, 1000).await,
            Acceptance::VersionRequired
        );
        assert_eq!(ledger(&db).await, (1, 1, 0));
    }
}
