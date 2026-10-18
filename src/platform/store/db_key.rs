//! The SQLCipher key for the gateway store: where it comes from, never what it is.
use std::path::{Path, PathBuf};

use crate::{StoreConfig, StoreEncryption};

pub(crate) const STORE_KEY_FILE: &str = "store.key";

#[derive(Clone)]
pub(crate) struct DbKey([u8; 32]);

impl std::fmt::Debug for DbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DbKey(..)")
    }
}

impl DbKey {
    pub(crate) fn random() -> DbKey {
        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        DbKey(bytes)
    }

    /// The raw-key form SQLCipher takes: `x'<64 hex>'`.
    pub(crate) fn pragma_value(&self) -> String {
        format!("x'{}'", hex::encode(self.0))
    }

    fn from_hex(text: &str) -> anyhow::Result<DbKey> {
        let bytes = hex::decode(text.trim())
            .map_err(|_| anyhow::anyhow!("store key file is not 64 hex characters"))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("store key file is not 64 hex characters"))?;
        Ok(DbKey(bytes))
    }
}

pub(crate) fn key_file(state: &Path) -> PathBuf {
    state.join(STORE_KEY_FILE)
}

pub(crate) fn load_or_create_file_key(state: &Path) -> anyhow::Result<DbKey> {
    let path = key_file(state);
    match std::fs::read_to_string(&path) {
        Ok(text) => DbKey::from_hex(&text).map_err(|e| anyhow::anyhow!("{e} ({})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A key minted next to a store somebody else keyed would only turn
            // "restore your key file" into a mystery, so refuse instead.
            let db = crate::store_db_path(state);
            anyhow::ensure!(
                !db.exists(),
                "store key file {} is missing but the database {} exists; restore the key file, the gateway never recreates an existing store",
                path.display(),
                db.display()
            );
            let key = DbKey::random();
            crate::write_secret_file(&path, format!("{}\n", hex::encode(key.0)).as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    }
}

pub(crate) fn key_for(config: &StoreConfig, state: &Path) -> anyhow::Result<Option<DbKey>> {
    match config.encryption {
        StoreEncryption::Off => Ok(None),
        StoreEncryption::File => load_or_create_file_key(state).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> PathBuf {
        let dir = crate::test_state_dir().join(format!("key-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_key_file_is_created_private_and_reused() {
        let dir = state("create");
        let first = load_or_create_file_key(&dir).unwrap();
        let path = key_file(&dir);
        assert_eq!(std::fs::read(&path).unwrap().len(), 65);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let second = load_or_create_file_key(&dir).unwrap();
        assert_eq!(first.pragma_value(), second.pragma_value());
        assert_eq!(first.pragma_value().len(), 2 + 64 + 1);
    }

    #[test]
    fn a_short_key_file_is_refused() {
        let dir = state("short");
        std::fs::write(key_file(&dir), "0123456789").unwrap();
        let err = load_or_create_file_key(&dir).unwrap_err();
        assert!(err.to_string().contains("64 hex characters"), "{err}");
    }

    #[test]
    fn a_missing_key_next_to_an_existing_store_is_not_replaced() {
        let dir = state("missing");
        let db = crate::store_db_path(&dir);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"existing").unwrap();
        let err = load_or_create_file_key(&dir).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("store.key") && text.contains("gateway.sqlite3"),
            "{text}"
        );
        assert!(!key_file(&dir).exists(), "no new key was minted");
    }

    #[test]
    fn encryption_off_has_no_key() {
        let dir = state("off");
        let config = StoreConfig {
            encryption: StoreEncryption::Off,
        };
        assert!(key_for(&config, &dir).unwrap().is_none());
        assert!(!key_file(&dir).exists());
    }
}
