//! Durable request identities and monotonic transport versions, scoped to the
//! current credential. Legacy envelopes cannot prove their timestamp: retain
//! their accepted nonces until the credential upgrades, never age them out.
use std::sync::Arc;

use rusqlite::{params, OptionalExtension as _};

use crate::platform::store::sqlite::{Migration, SqliteStore};

use super::transport::{MAX_CLOCK_SKEW_MS, VERSION_V2};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "transport",
    steps: &["
CREATE TABLE transport_credentials (
  device_id TEXT NOT NULL, credential_hash TEXT NOT NULL,
  minimum_version INTEGER NOT NULL,
  PRIMARY KEY(device_id, credential_hash));
CREATE TABLE transport_nonces (
  device_id TEXT NOT NULL, credential_hash TEXT NOT NULL, nonce BLOB NOT NULL,
  version INTEGER NOT NULL, timestamp_ms INTEGER NOT NULL,
  PRIMARY KEY(device_id, credential_hash, nonce));
CREATE INDEX transport_nonces_expiry ON transport_nonces(version, timestamp_ms);
"],
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
}

pub(crate) async fn accept(
    db: &Arc<SqliteStore>,
    device_id: String,
    credential_hash: String,
    nonce: [u8; 12],
    version: u8,
    timestamp_ms: u128,
    now_ms: u128,
) -> anyhow::Result<Acceptance> {
    let timestamp = i64::try_from(timestamp_ms)?;
    let oldest = i64::try_from(now_ms.saturating_sub(MAX_CLOCK_SKEW_MS))?;
    db.call(move |connection| {
        let tx = connection.transaction()?;
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
            now,
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
                1002
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
}
