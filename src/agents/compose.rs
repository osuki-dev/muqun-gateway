//! Composing the agents plane: the one place that turns configuration into a
//! running [`AgentRuntime`] and attaches the shared store to it.
//!
//! The headless gateway (`platform::server`), its `agent` command and an
//! embedding desktop host all build the runtime here, so none of them can
//! drift from the others.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::runtime::{DeepseekConfig, LocalAgentConfigs, OpencodeConfig, T3Config};
use super::AgentRuntime;
use crate::platform::store::sqlite::SqliteStore;

/// Every agent adapter's configuration, as `config.json` names them.
#[derive(Clone, Debug, Default)]
pub struct RuntimeSettings {
    pub opencode: OpencodeConfig,
    pub deepseek: DeepseekConfig,
    pub t3: T3Config,
    pub local: LocalAgentConfigs,
}

impl RuntimeSettings {
    pub(crate) fn from_config(config: &crate::Config) -> Self {
        Self {
            opencode: config.opencode.clone(),
            deepseek: config.deepseek.clone(),
            t3: config.t3.clone(),
            local: LocalAgentConfigs {
                codex: config.codex.clone(),
                fx: config.fx.clone(),
                qoder: config.qoder.clone(),
                antigravity: config.antigravity.clone(),
                cursor: config.cursor.clone(),
                pi: config.pi.clone(),
                claude: config.claude.clone(),
            },
        }
    }
}

/// Build the agent runtime, or a disabled one when `enabled` is false.
pub fn build_runtime(
    settings: RuntimeSettings,
    enabled: bool,
    state_dir: Option<PathBuf>,
) -> Arc<AgentRuntime> {
    if !enabled {
        return AgentRuntime::disabled();
    }
    AgentRuntime::with_local_configs(
        settings.opencode,
        settings.deepseek,
        settings.t3,
        settings.local,
        state_dir,
    )
}

/// Whether the shared SQLite store is encrypted at rest. Mirrors
/// `platform::config::StoreEncryption`, which is not public.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreEncryption {
    /// Plain SQLite.
    Off,
    /// SQLCipher, keyed by a key file in the state directory.
    File,
}

impl From<crate::platform::config::StoreEncryption> for StoreEncryption {
    fn from(value: crate::platform::config::StoreEncryption) -> Self {
        match value {
            crate::platform::config::StoreEncryption::Off => Self::Off,
            crate::platform::config::StoreEncryption::File => Self::File,
        }
    }
}

impl From<StoreEncryption> for crate::platform::config::StoreEncryption {
    fn from(value: StoreEncryption) -> Self {
        match value {
            StoreEncryption::Off => Self::Off,
            StoreEncryption::File => Self::File,
        }
    }
}

/// Open the shared store in `state_dir`, creating it and its key as needed.
pub(crate) fn open_store(
    state_dir: &Path,
    encryption: StoreEncryption,
) -> anyhow::Result<Arc<SqliteStore>> {
    let config = crate::platform::config::StoreConfig {
        encryption: encryption.into(),
    };
    let key = crate::platform::store::db_key::key_for(&config, state_dir)?;
    SqliteStore::open(
        &crate::platform::store::store_db_path(state_dir),
        key,
        crate::platform::store::sqlite::ALL_MIGRATIONS,
    )
    .map_err(|err| match encryption {
        // One sentence that names both files, so the owner knows what to restore.
        StoreEncryption::File => anyhow::anyhow!(
            "{err} (key file: {})",
            crate::platform::store::db_key::key_file(state_dir).display()
        ),
        StoreEncryption::Off => err,
    })
}

/// Attach the durable store to `runtime`: open it, then persist T3
/// credentials (only when encrypted), restore native Codex sessions and
/// persist session routes.
pub async fn attach_store(
    runtime: &Arc<AgentRuntime>,
    state_dir: &Path,
    encryption: StoreEncryption,
) -> anyhow::Result<()> {
    let db = open_store(state_dir, encryption)?;
    attach_opened_store(runtime, &db, encryption).await;
    Ok(())
}

/// [`attach_store`] for a caller that keeps the store for itself.
pub(crate) async fn attach_opened_store(
    runtime: &Arc<AgentRuntime>,
    db: &Arc<SqliteStore>,
    encryption: StoreEncryption,
) {
    if encryption == StoreEncryption::File {
        runtime.persist_t3_credentials(db.clone());
    }
    runtime.restore_native_codex_sessions(db.clone());
    runtime.persist_session_routes(db.clone()).await;
}
