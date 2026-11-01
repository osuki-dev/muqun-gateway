//! What a new session gets when the App does not pick: the model (with its
//! effort in `variant`) and mode last chosen for that agent in that folder,
//! else on this computer, kept in the shared store so every device sees the
//! same. And, for agents whose model list only exists inside a live session
//! (ACP agents), the last list a session showed, so a new session can pick a
//! model and effort before its first prompt.
//!
//! Only choices the App makes are recorded: a model or mode named when a
//! session is created, and a later switch. Nothing typed into an agent on the
//! computer is. Only ids are kept, never session content.

use std::sync::Mutex;

use rusqlite::OptionalExtension as _;

use crate::agents::domain::{AgentCatalog, CatalogDefaults, ModelInfo, ModelRef};
use crate::platform::store::sqlite::{Migration, SqliteStore};

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "agent_session_defaults",
    steps: &["CREATE TABLE agent_choices (
            agent_id TEXT NOT NULL,
            cwd TEXT NOT NULL,
            provider_id TEXT,
            model_id TEXT,
            variant TEXT,
            mode TEXT,
            updated_ms INTEGER NOT NULL,
            PRIMARY KEY (agent_id, cwd)
        );
        CREATE TABLE agent_catalog_cache (
            agent_id TEXT PRIMARY KEY,
            version TEXT NOT NULL,
            models TEXT NOT NULL,
            updated_ms INTEGER NOT NULL
        );"],
};

/// The row that stands for "the last choice on this computer".
const COMPUTER: &str = "";
/// Folders remembered per agent, besides the computer-wide row.
const MAX_FOLDERS: i64 = 64;

/// The folder a choice is remembered under: the real path when it exists.
fn folder_key(directory: &str) -> String {
    std::fs::canonicalize(directory)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| directory.to_owned())
}

/// Remember what the App chose for `agent_id` in `directory`: a model (its
/// `variant` is the effort), a mode, or both. `None` leaves that part as it
/// was. Failures are logged, never returned: remembering is a convenience.
pub(crate) async fn record(
    db: &SqliteStore,
    agent_id: &str,
    directory: Option<&str>,
    model: Option<&ModelRef>,
    mode: Option<&str>,
) {
    if model.is_none() && mode.is_none() {
        return;
    }
    let agent_id = agent_id.to_owned();
    let folder = directory.map(folder_key);
    let model = model.cloned();
    let mode = mode.map(str::to_owned);
    let now = crate::tasks::domain::now_ms() as i64;
    let written = db
        .call(move |db| {
            let tx = db.transaction()?;
            for cwd in folder.iter().map(String::as_str).chain([COMPUTER]) {
                tx.execute(
                    "INSERT INTO agent_choices(agent_id, cwd, updated_ms) VALUES(?1, ?2, ?3)
                     ON CONFLICT(agent_id, cwd) DO UPDATE SET updated_ms = excluded.updated_ms",
                    rusqlite::params![agent_id, cwd, now],
                )?;
                if let Some(model) = &model {
                    tx.execute(
                        "UPDATE agent_choices SET provider_id = ?3, model_id = ?4, variant = ?5
                         WHERE agent_id = ?1 AND cwd = ?2",
                        rusqlite::params![
                            agent_id,
                            cwd,
                            model.provider_id,
                            model.model_id,
                            model.variant
                        ],
                    )?;
                }
                if let Some(mode) = &mode {
                    tx.execute(
                        "UPDATE agent_choices SET mode = ?3 WHERE agent_id = ?1 AND cwd = ?2",
                        rusqlite::params![agent_id, cwd, mode],
                    )?;
                }
            }
            tx.execute(
                "DELETE FROM agent_choices WHERE agent_id = ?1 AND cwd != '' AND cwd NOT IN
                 (SELECT cwd FROM agent_choices WHERE agent_id = ?1 AND cwd != ''
                  ORDER BY updated_ms DESC LIMIT ?2)",
                rusqlite::params![agent_id, MAX_FOLDERS],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(error) = written {
        tracing::debug!("session choice not remembered: {error:#}");
    }
}

/// One remembered row: model and mode, each possibly unset.
type Choice = (Option<ModelRef>, Option<String>);

async fn choices(db: &SqliteStore, agent_id: &str, folder: Option<String>) -> Vec<Choice> {
    let agent_id = agent_id.to_owned();
    db.call(move |db| {
        let mut rows = Vec::new();
        for cwd in folder.iter().map(String::as_str).chain([COMPUTER]) {
            let row = db
                .query_row(
                    "SELECT provider_id, model_id, variant, mode FROM agent_choices
                     WHERE agent_id = ?1 AND cwd = ?2",
                    rusqlite::params![agent_id, cwd],
                    |row| {
                        let provider: Option<String> = row.get(0)?;
                        let model: Option<String> = row.get(1)?;
                        let variant: Option<String> = row.get(2)?;
                        let model = provider.zip(model).map(|(provider_id, model_id)| ModelRef {
                            provider_id,
                            model_id,
                            variant,
                        });
                        Ok((model, row.get::<_, Option<String>>(3)?))
                    },
                )
                .optional()?;
            rows.extend(row);
        }
        Ok(rows)
    })
    .await
    .unwrap_or_default()
}

/// Modes that widen what the agent may do unasked (Claude Code's). A new
/// session gets one only when it asks and its own live mode list offers it
/// (ACP `create_session`), never as a remembered default: before a session
/// exists, the list it will offer is unknown.
pub(crate) const ELEVATING_MODES: [&str; 2] = ["bypassPermissions", "auto"];

/// The catalog's defaults with the remembered choice in front: this folder's
/// last, then this computer's last, then what the agent itself reported. A
/// remembered model or mode the catalog no longer offers is skipped, and so
/// is an effort the model no longer offers and an elevating mode.
pub(crate) async fn fill_defaults(
    db: &SqliteStore,
    agent_id: &str,
    directory: Option<&str>,
    catalog: &mut AgentCatalog,
) {
    let remembered = choices(db, agent_id, directory.map(folder_key)).await;
    catalog.defaults = pick_defaults(&remembered, catalog);
}

fn pick_defaults(remembered: &[Choice], catalog: &AgentCatalog) -> CatalogDefaults {
    let offered_model = |choice: &ModelRef| {
        let model = catalog
            .models
            .iter()
            .find(|m| m.provider_id == choice.provider_id && m.id == choice.model_id)?;
        let variant = choice.variant.as_ref().filter(|variant| {
            model
                .variants
                .iter()
                .flatten()
                .any(|offered| &offered.id == *variant)
        });
        Some(ModelRef {
            variant: variant.cloned(),
            ..choice.clone()
        })
    };
    let model = remembered
        .iter()
        .filter_map(|(model, _)| model.as_ref())
        .find_map(offered_model)
        .or_else(|| catalog.defaults.model.clone());
    let mode = remembered
        .iter()
        .filter_map(|(_, mode)| mode.as_ref())
        .filter(|mode| !ELEVATING_MODES.contains(&mode.as_str()))
        .find(|mode| catalog.modes.iter().any(|offered| &offered.id == *mode))
        .cloned()
        .or_else(|| catalog.defaults.mode.clone());
    CatalogDefaults { model, mode }
}

/// What was last written to the catalog cache per agent, so an unchanged
/// list is not written again on every catalog read.
static WRITTEN: Mutex<Vec<(String, String, u64)>> = Mutex::new(Vec::new());

fn fingerprint(version: &str, models: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    version.hash(&mut hasher);
    models.hash(&mut hasher);
    hasher.finish()
}

/// A catalog read before any session (cached or last seen) carries no
/// current model. The adapter's own default then stands, by the name the
/// adapter gives it -- Claude Code's `default` ("Default (recommended)"),
/// Cursor's `default[]` ("Auto") -- or the one model offered (Antigravity),
/// rather than whichever model the App lists first.
pub(crate) fn fill_adapter_default(catalog: &mut AgentCatalog) {
    if catalog.defaults.model.is_some() {
        return;
    }
    let own = catalog
        .models
        .iter()
        .find(|model| model.id == "default" || model.id.starts_with("default["));
    let only = match catalog.models.as_slice() {
        [model] => Some(model),
        _ => None,
    };
    catalog.defaults.model = own.or(only).map(|model| ModelRef {
        provider_id: model.provider_id.clone(),
        model_id: model.id.clone(),
        variant: None,
    });
}

/// Keep the model list an agent shows while a session is open, and give a
/// catalog that has none (no session yet) the last one seen from the same
/// agent version. A different version drops the cached list: its models and
/// efforts may have changed. Agents without a version are not cached.
pub(crate) async fn with_cached_models(
    db: &SqliteStore,
    agent_id: &str,
    version: Option<String>,
    catalog: &mut AgentCatalog,
) {
    let Some(version) = version else {
        return;
    };
    if catalog.models.is_empty() {
        if let Some(models) = cached_models(db, agent_id, &version).await {
            catalog.models = models;
        }
        return;
    }
    let Ok(models) = serde_json::to_string(&catalog.models) else {
        return;
    };
    let print = fingerprint(&version, &models);
    {
        let written = WRITTEN.lock().unwrap();
        if written
            .iter()
            .any(|(agent, _, known)| agent == agent_id && *known == print)
        {
            return;
        }
    }
    let (agent, at) = (agent_id.to_owned(), crate::tasks::domain::now_ms() as i64);
    let stored_version = version.clone();
    let fresh = catalog.models.clone();
    let stored = db
        .call(move |db| {
            // A session lists effort tiers only for its selected model; keep
            // the tiers earlier sessions showed for the others.
            let earlier: Option<String> = db
                .query_row(
                    "SELECT models FROM agent_catalog_cache WHERE agent_id = ?1 AND version = ?2",
                    rusqlite::params![agent, stored_version],
                    |row| row.get(0),
                )
                .optional()?;
            let earlier: Vec<ModelInfo> = earlier
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default();
            let models = serde_json::to_string(&merge_variants(fresh, &earlier))?;
            db.execute(
                "INSERT INTO agent_catalog_cache(agent_id, version, models, updated_ms)
                 VALUES(?1, ?2, ?3, ?4) ON CONFLICT(agent_id) DO UPDATE SET
                 version = excluded.version, models = excluded.models,
                 updated_ms = excluded.updated_ms",
                rusqlite::params![agent, stored_version, models, at],
            )?;
            Ok(())
        })
        .await;
    match stored {
        Ok(()) => {
            let mut written = WRITTEN.lock().unwrap();
            written.retain(|(agent, _, _)| agent != agent_id);
            written.push((agent_id.to_owned(), version, print));
        }
        Err(error) => tracing::debug!("agent catalog not cached: {error:#}"),
    }
}

/// `fresh` with the effort tiers `earlier` knew for a model `fresh` lists
/// without any.
fn merge_variants(mut fresh: Vec<ModelInfo>, earlier: &[ModelInfo]) -> Vec<ModelInfo> {
    for model in &mut fresh {
        if model.variants.as_ref().is_none_or(Vec::is_empty) {
            model.variants = earlier
                .iter()
                .find(|old| old.provider_id == model.provider_id && old.id == model.id)
                .and_then(|old| old.variants.clone());
        }
    }
    fresh
}

/// A model asked for while the agent lists none (an ACP agent before its
/// first session, with nothing cached): passed on as asked, for the agent
/// to accept or refuse. `<provider>/<model>` names the provider; a bare id
/// is in the agent's own namespace.
pub(crate) fn unlisted_model(agent_id: &str, id: &str, variant: Option<&str>) -> ModelRef {
    let (provider_id, model_id) = id
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .unwrap_or((agent_id, id));
    ModelRef {
        provider_id: provider_id.into(),
        model_id: model_id.into(),
        variant: variant.map(str::to_owned),
    }
}

/// After a session was created: remember what the App chose, and the model
/// list the new session shows, so the next new session (also after a
/// restart) can choose before its first prompt.
pub(crate) async fn after_create(
    db: &SqliteStore,
    manager: &crate::agents::manager::AgentManager,
    asid: &str,
    directory: Option<&str>,
    model: Option<&ModelRef>,
    mode: Option<&str>,
) {
    let agent_id = manager.agent().kind();
    record(db, agent_id, directory, model, mode).await;
    if let Ok(mut catalog) = manager
        .sessions()
        .get_catalog_for_session(Some(asid), directory)
        .await
    {
        with_cached_models(db, agent_id, manager.version(), &mut catalog).await;
    }
}

pub(crate) async fn cached_models(
    db: &SqliteStore,
    agent_id: &str,
    version: &str,
) -> Option<Vec<ModelInfo>> {
    let (agent, version) = (agent_id.to_owned(), version.to_owned());
    let text: Option<String> = db
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT models FROM agent_catalog_cache WHERE agent_id = ?1 AND version = ?2",
                    rusqlite::params![agent, version],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
        .ok()
        .flatten();
    let mut models: Vec<ModelInfo> = serde_json::from_str(&text?).ok()?;
    // Lists cached before efforts were ordered low to high.
    for variants in models
        .iter_mut()
        .filter_map(|model| model.variants.as_mut())
    {
        crate::agents::domain::order_efforts(variants, |variant| {
            variant.reasoning_effort.as_deref()
        });
    }
    Some(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> std::sync::Arc<SqliteStore> {
        SqliteStore::open(
            &crate::test_state_dir().join(format!("{}.sqlite3", uuid::Uuid::new_v4())),
            None,
            crate::platform::store::sqlite::ALL_MIGRATIONS,
        )
        .unwrap()
    }

    fn model(id: &str, variant: Option<&str>) -> ModelRef {
        ModelRef {
            provider_id: "p".into(),
            model_id: id.into(),
            variant: variant.map(Into::into),
        }
    }

    fn catalog() -> AgentCatalog {
        serde_json::from_value(serde_json::json!({
            "models": [
                {"id": "fast", "name": "Fast", "provider_id": "p"},
                {"id": "deep", "name": "Deep", "provider_id": "p",
                 "variants": [{"id": "low", "reasoning_effort": "low"}, {"id": "high", "reasoning_effort": "high"}]}
            ],
            "modes": [{"id": "build", "name": "Build"}, {"id": "plan", "name": "Plan"}],
            "mcp": [],
            "defaults": {"model": {"provider_id": "p", "model_id": "fast"}, "mode": "build"}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn defaults_come_from_the_folder_then_the_computer_then_the_agent() {
        let db = store();
        let dir = |name: &str| {
            let path = crate::test_state_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            path.to_string_lossy().into_owned()
        };
        let (a, b, c) = (dir("a"), dir("b"), dir("c"));
        let agent = format!("agent-{}", uuid::Uuid::new_v4());
        // Nothing remembered: the agent's own default.
        let mut fresh = catalog();
        fill_defaults(&db, &agent, Some(&a), &mut fresh).await;
        assert_eq!(fresh.defaults, catalog().defaults);

        record(
            &db,
            &agent,
            Some(&a),
            Some(&model("deep", Some("high"))),
            None,
        )
        .await;
        record(&db, &agent, Some(&b), None, Some("plan")).await;
        for (folder, want_model, want_mode) in [
            // A's own model; no mode in A, so the computer's last (plan).
            (&a, model("deep", Some("high")), "plan"),
            // B named only a mode: the model is the computer's last.
            (&b, model("deep", Some("high")), "plan"),
            // A folder never used: the computer's last.
            (&c, model("deep", Some("high")), "plan"),
        ] {
            let mut cat = catalog();
            fill_defaults(&db, &agent, Some(folder), &mut cat).await;
            assert_eq!(cat.defaults.model, Some(want_model), "{folder}");
            assert_eq!(cat.defaults.mode.as_deref(), Some(want_mode), "{folder}");
        }
        // A model the agent no longer offers is skipped: B's and the
        // computer's last are gone, so B falls back to the agent's default,
        // while A keeps its own.
        record(&db, &agent, Some(&b), Some(&model("gone", None)), None).await;
        let mut cat = catalog();
        fill_defaults(&db, &agent, Some(&b), &mut cat).await;
        assert_eq!(cat.defaults.model, Some(model("fast", None)));
        let mut cat = catalog();
        fill_defaults(&db, &agent, Some(&a), &mut cat).await;
        assert_eq!(cat.defaults.model, Some(model("deep", Some("high"))));
        let wire = serde_json::to_value(&{
            let mut cat = catalog();
            fill_defaults(&db, &agent, Some(&a), &mut cat).await;
            cat.defaults
        })
        .unwrap();
        assert_eq!(wire["effort"], "high", "effort repeats model.variant");

        // An effort the model no longer offers is dropped, the model kept.
        record(
            &db,
            &agent,
            Some(&a),
            Some(&model("deep", Some("max"))),
            None,
        )
        .await;
        let mut cat = catalog();
        fill_defaults(&db, &agent, Some(&a), &mut cat).await;
        assert_eq!(cat.defaults.model, Some(model("deep", None)));
    }

    #[test]
    fn a_remembered_elevating_mode_is_never_a_new_session_s_default() {
        let mut offered = catalog();
        offered.modes = serde_json::from_value(serde_json::json!([
            {"id": "default", "name": "Manual"}, {"id": "plan", "name": "Plan"},
            {"id": "auto", "name": "Auto"}, {"id": "bypassPermissions", "name": "Bypass"}
        ]))
        .unwrap();
        offered.defaults.mode = Some("default".into());
        for (remembered, want) in [
            (vec!["bypassPermissions"], "default"),
            (vec!["auto"], "default"),
            // The folder's elevating choice is passed over for the computer's.
            (vec!["bypassPermissions", "plan"], "plan"),
            (vec!["plan"], "plan"),
        ] {
            let remembered: Vec<Choice> = remembered
                .into_iter()
                .map(|mode| (None, Some(mode.to_owned())))
                .collect();
            let picked = pick_defaults(&remembered, &offered);
            assert_eq!(picked.mode.as_deref(), Some(want), "{remembered:?}");
        }
    }

    #[tokio::test]
    async fn a_live_sessions_models_serve_a_new_session_of_the_same_version() {
        let db = store();
        let agent = format!("acp-{}", uuid::Uuid::new_v4());
        let mut live = catalog();
        with_cached_models(&db, &agent, Some("1.0".into()), &mut live).await;
        for (version, cached) in [(Some("1.0"), true), (Some("2.0"), false), (None, false)] {
            let mut empty = AgentCatalog::default();
            with_cached_models(&db, &agent, version.map(Into::into), &mut empty).await;
            assert_eq!(!empty.models.is_empty(), cached, "{version:?}");
        }
    }

    #[tokio::test]
    async fn the_cached_list_survives_a_restart_and_keeps_each_models_tiers() {
        let path = crate::test_state_dir().join(format!("{}.sqlite3", uuid::Uuid::new_v4()));
        let open = || {
            SqliteStore::open(&path, None, crate::platform::store::sqlite::ALL_MIGRATIONS).unwrap()
        };
        let agent = format!("claude-{}", uuid::Uuid::new_v4());
        let listed = |selected: &str| -> AgentCatalog {
            let tiers = serde_json::json!([{"id": "low", "reasoning_effort": "low"}, {"id": "high", "reasoning_effort": "high"}]);
            let model = |id: &str| {
                let mut model = serde_json::json!({"id": id, "name": id, "provider_id": "claude"});
                if id == selected {
                    model["variants"] = tiers.clone();
                }
                model
            };
            serde_json::from_value(serde_json::json!({
                "models": [model("opus"), model("haiku")], "modes": [], "mcp": []
            }))
            .unwrap()
        };
        {
            let db = open();
            // Two sessions, each showing tiers only for its selected model.
            with_cached_models(&db, &agent, Some("2.1".into()), &mut listed("opus")).await;
            with_cached_models(&db, &agent, Some("2.1".into()), &mut listed("haiku")).await;
        }
        // A new process: nothing in memory, the store file reopened.
        let db = open();
        let mut empty = AgentCatalog::default();
        with_cached_models(&db, &agent, Some("2.1".into()), &mut empty).await;
        let tiers = |id: &str| {
            empty
                .models
                .iter()
                .find(|model| model.id == id)
                .and_then(|model| model.variants.as_ref())
                .map(|variants| variants.len())
        };
        assert_eq!((tiers("opus"), tiers("haiku")), (Some(2), Some(2)));
        let discovered: Vec<_> = empty
            .models
            .iter()
            .map(crate::discovery::model_info_to_agent_model)
            .collect();
        assert!(discovered
            .iter()
            .all(|model| model.reasoning_effort_tiers == ["low", "high"]));
    }

    /// Before a session, the default is what the adapter runs unasked, not
    /// the first model in the list (`default` is not always listed first).
    #[test]
    fn a_catalog_without_a_current_model_defaults_to_the_adapter_s_own() {
        let listed = |ids: &[&str]| -> AgentCatalog {
            let models: Vec<_> = ids
                .iter()
                .map(|id| serde_json::json!({"id": id, "name": id, "provider_id": "a"}))
                .collect();
            serde_json::from_value(serde_json::json!({"models": models, "modes": [], "mcp": []}))
                .unwrap()
        };
        let chosen = model("haiku", Some("low"));
        for (ids, current, want) in [
            (
                &["claude-fable-5", "default", "haiku"][..],
                None,
                Some("default"),
            ),
            (
                &["default[]", "gpt-5.5[reasoning=medium]"][..],
                None,
                Some("default[]"),
            ),
            (&["gemini-3.8-flash"][..], None, Some("gemini-3.8-flash")),
            (&["fast", "deep"][..], None, None),
            (
                &["default", "haiku"][..],
                Some(chosen.clone()),
                Some("haiku"),
            ),
        ] {
            let mut catalog = listed(ids);
            catalog.defaults.model = current;
            fill_adapter_default(&mut catalog);
            assert_eq!(
                catalog.defaults.model.as_ref().map(|m| m.model_id.as_str()),
                want,
                "{ids:?}"
            );
        }
    }

    #[test]
    fn a_model_the_agent_does_not_list_is_passed_on_as_asked() {
        for (id, variant, provider, model) in [
            ("haiku", Some("medium"), "claude", "haiku"),
            (
                "anthropic/claude-haiku-5-5",
                None,
                "anthropic",
                "claude-haiku-5-5",
            ),
            ("/odd", None, "claude", "/odd"),
        ] {
            let got = unlisted_model("claude", id, variant);
            assert_eq!(
                (
                    got.provider_id.as_str(),
                    got.model_id.as_str(),
                    got.variant.as_deref()
                ),
                (provider, model, variant),
                "{id}"
            );
        }
    }
}
