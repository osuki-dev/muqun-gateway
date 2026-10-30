//! Read-only, bounded official FX JSON commands. No model request or profile
//! mutation is used to discover choices or provider-account rate windows.
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agents::domain::account_limits::{
    AccountLimit, AccountLimitWindow, AccountLimits, Credits,
};
use crate::agents::domain::{AgentCatalog, CatalogDefaults, ModelInfo, ModelRef};
use crate::agents::ports::agent::AgentError;

pub(crate) async fn read_json(
    binary: &str,
    arguments: &[&str],
    directory: &str,
    stop: &CancellationToken,
) -> Result<Value, AgentError> {
    let bytes = crate::agents::adapters::acp::session::read_metadata_output(
        binary, arguments, directory, stop,
    )
    .await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| AgentError::Protocol("FX metadata was not JSON".into()))
}

pub(crate) async fn recovery_snapshot(
    binary: &str,
    directory: &str,
    id: &str,
    stop: &CancellationToken,
) -> Result<crate::agents::adapters::acp::recovery::NativeSnapshot, AgentError> {
    let bytes = crate::agents::adapters::acp::session::read_metadata_output_bounded(
        binary,
        &["session", "--id", id, "--json"],
        directory,
        stop,
        64 * 1024 * 1024,
        std::time::Duration::from_secs(3),
    )
    .await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AgentError::Protocol("FX recovery metadata was not JSON".into()))?;
    drop(bytes);
    recovery_snapshot_value(id, value)
}

fn recovery_snapshot_value(
    id: &str,
    mut value: Value,
) -> Result<crate::agents::adapters::acp::recovery::NativeSnapshot, AgentError> {
    use crate::agents::adapters::acp::recovery::NativeSnapshot;
    if value["kind"].as_str() != Some("session_detail") || value["id"].as_str() != Some(id) {
        return Err(AgentError::Protocol(
            "FX recovery metadata changed session identity".into(),
        ));
    }
    let count = value["history_len"].as_u64();
    let entries = match value["history"].take() {
        Value::Array(entries) => entries,
        _ => {
            return Err(AgentError::Protocol(
                "FX recovery history is missing".into(),
            ))
        }
    };
    if count != Some(entries.len() as u64) || entries.len() > 100_000 {
        return Err(AgentError::Protocol(
            "FX recovery history is incomplete or exceeds its budget".into(),
        ));
    }
    let last = entries
        .last()
        .filter(|turn| turn["kind"].as_str() == Some("assistant"));
    let last_user = last
        .and_then(|turn| turn["user"]["text"].as_str())
        .map(str::to_owned);
    let last_assistant = last
        .and_then(|turn| turn["assistant"].as_str())
        .map(str::to_owned);
    Ok(NativeSnapshot {
        session_id: id.into(),
        entries,
        last_user,
        last_assistant,
    })
}

fn native_provider<'a>(source: &str, status: &'a Value) -> Option<&'a str> {
    let provider = match source {
        "Codex subscription" => Some("codex"),
        "Grok subscription" => Some("grok"),
        "Vercel AI Gateway" | "AI Gateway" => Some("gateway"),
        _ => None,
    };
    let connected = status["connected_providers"].as_array()?;
    if let Some(provider) = provider {
        return connected
            .iter()
            .filter_map(Value::as_str)
            .find(|id| *id == provider);
    }
    let mut matching = connected
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| *id == source || connected.len() == 1);
    let provider = matching.next()?;
    matching.next().is_none().then_some(provider)
}

pub(crate) fn catalog(models: &Value, status: &Value) -> Result<AgentCatalog, AgentError> {
    if models["kind"].as_str() != Some("models") || status["kind"].as_str() != Some("status") {
        return Err(AgentError::Protocol(
            "FX metadata has an unknown schema".into(),
        ));
    }
    let mut catalog = AgentCatalog::default();
    for model in models["models"]
        .as_array()
        .ok_or_else(|| AgentError::Protocol("FX model metadata missing models".into()))?
    {
        let id = model["id"]
            .as_str()
            .ok_or_else(|| AgentError::Protocol("FX model metadata missing id".into()))?;
        let provider = native_provider(model["source"].as_str().unwrap_or_default(), status)
            .ok_or_else(|| AgentError::NotAvailable("FX did not identify the native model provider; open a session to read its catalog".into()))?;
        catalog.models.push(ModelInfo {
            id: id.into(),
            name: model["name"].as_str().unwrap_or(id).into(),
            provider_id: provider.into(),
            family: None,
            limit: None,
            variants: None,
            cost: None,
            enabled: true,
            status: None,
        });
    }
    let selected = status["model"]
        .as_str()
        .and_then(|id| catalog.models.iter().find(|model| model.id == id));
    catalog.defaults = CatalogDefaults {
        model: selected.map(|model| ModelRef {
            provider_id: model.provider_id.clone(),
            model_id: model.id.clone(),
            variant: None,
        }),
        mode: None,
    };
    Ok(catalog)
}

pub(crate) async fn read_catalog(
    binary: &str,
    directory: &str,
    stop: &CancellationToken,
) -> Result<AgentCatalog, AgentError> {
    let models = read_json(binary, &["models", "--json"], directory, stop).await?;
    let status = read_json(binary, &["status", "--json"], directory, stop).await?;
    catalog(&models, &status)
}

fn decimal(value: &str) -> bool {
    if value.len() > 256 {
        return false;
    }
    let value = value.strip_prefix('-').unwrap_or(value);
    let mut parts = value.split('.');
    let integer = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let digits = |value: &str| value.bytes().all(|byte| byte.is_ascii_digit());
    (!integer.is_empty() || fraction.is_some_and(|fraction| !fraction.is_empty()))
        && digits(integer)
        && fraction.is_none_or(digits)
}

pub(crate) fn account_limits(value: &Value) -> AccountLimits {
    if value["kind"].as_str() != Some("usage")
        || value["status"].as_str() != Some("ready")
        || value["source"].as_str() != Some("chatgpt")
    {
        return AccountLimits::unavailable("temporarily_unavailable");
    }
    let codex = &value["codex"];
    let mut limits = Vec::new();
    for (index, native) in codex["limits"].as_array().into_iter().flatten().enumerate() {
        let mut windows = Vec::new();
        for id in ["primary", "secondary"] {
            let window = &native[id];
            let Some(used) = window["used_percent"]
                .as_f64()
                .filter(|used| used.is_finite() && *used >= 0.0)
            else {
                continue;
            };
            windows.push(AccountLimitWindow {
                id: id.into(),
                label: None,
                unit: "percent".into(),
                used: Some(used),
                limit: Some(100.0),
                remaining: Some((100.0 - used).max(0.0)),
                window_duration_minutes: window["window_seconds"]
                    .as_u64()
                    .filter(|seconds| *seconds >= 60 && seconds % 60 == 0)
                    .map(|seconds| seconds / 60),
                resets_at_ms: window["resets_at_ms"]
                    .as_u64()
                    .filter(|timestamp| *timestamp <= 8_640_000_000_000_000),
            });
        }
        if windows.is_empty() {
            continue;
        }
        limits.push(AccountLimit {
            id: format!("fx-codex-{index}"),
            name: native["name"].as_str().map(str::to_owned),
            plan_type: codex["plan"].as_str().map(str::to_owned),
            model_id: None,
            credits: codex["credits"].is_object().then(|| Credits {
                has_credits: codex["credits"]["has_credits"].as_bool(),
                unlimited: codex["credits"]["unlimited"].as_bool(),
                balance: codex["credits"]["balance"]
                    .as_str()
                    .filter(|value| decimal(value))
                    .map(str::to_owned),
            }),
            individual_limit: None,
            spend_control_reached: codex["spend_limit_reached"].as_bool(),
            limit_reached_reason: None,
            windows,
        });
    }
    if limits.is_empty() {
        return AccountLimits::unavailable("temporarily_unavailable");
    }
    AccountLimits {
        status: "available",
        reason: None,
        observed_at_ms: value["fetched_at_ms"]
            .as_u64()
            .filter(|timestamp| *timestamp <= 8_640_000_000_000_000)
            .unwrap_or_else(crate::tasks::domain::now_ms),
        ordinary_usage_allowed: codex["spend_limit_reached"]
            .as_bool()
            .and_then(|reached| reached.then_some(false)),
        reset_credits: None,
        limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    #[ignore = "requires explicit FX_RECOVERY_QA_ID belonging to an authorized completed QA session; read-only"]
    async fn real_fx_recovery_snapshot_read_only() {
        let id = std::env::var("FX_RECOVERY_QA_ID").expect("explicit own QA identity required");
        let binary = std::env::var("FX_RECOVERY_QA_BINARY").unwrap_or_else(|_| "fx".into());
        let cwd = std::env::current_dir().unwrap();
        let snapshot = recovery_snapshot(
            &binary,
            cwd.to_str().unwrap(),
            &id,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(snapshot.session_id, id);
        assert!(snapshot
            .last_user
            .as_ref()
            .is_some_and(|text| !text.is_empty()));
        assert!(snapshot
            .last_assistant
            .as_ref()
            .is_some_and(|text| !text.is_empty()));
        println!(
            "verified native saved history: turns={}, checkpoint={}",
            snapshot.entries.len(),
            snapshot.checkpoint().digest
        );
    }

    #[test]
    fn recovery_requires_exact_identity_complete_history_and_final_normal_turn() {
        let good = json_for_recovery(
            "same",
            serde_json::json!([{"kind":"assistant","user":{"text":"question","images":[]},"assistant":"answer","execution":{"tool_steps":[]}}]),
        );
        let snapshot = recovery_snapshot_value("same", good.clone()).unwrap();
        assert_eq!(snapshot.last_assistant.as_deref(), Some("answer"));
        assert!(recovery_snapshot_value("other", good.clone()).is_err());
        let mut truncated = good;
        truncated["history_len"] = serde_json::json!(2);
        assert!(recovery_snapshot_value("same", truncated).is_err());
        let cancelled = recovery_snapshot_value(
            "same",
            json_for_recovery("same", serde_json::json!([{"kind":"interrupted"}])),
        )
        .unwrap();
        assert!(cancelled.last_assistant.is_none());
    }
    fn json_for_recovery(id: &str, history: Value) -> Value {
        serde_json::json!({"kind":"session_detail","id":id,"history_len":history.as_array().unwrap().len(),"history":history})
    }

    #[test]
    fn cold_catalog_uses_verified_provider_identity_without_guessing_effort() {
        let models =
            json!({"kind":"models","models":[{"id":"opaque-model","source":"Codex subscription"}]});
        let status = json!({"kind":"status","model":"opaque-model","connected_providers":["codex"],"effort":"high"});
        let result = catalog(&models, &status).unwrap();
        assert_eq!(result.models[0].provider_id, "codex");
        assert!(result.models[0].variants.is_none());
        assert!(result.defaults.model.unwrap().variant.is_none());
        assert!(catalog(
            &models,
            &json!({"kind":"status","connected_providers":["other"]})
        )
        .is_err());
    }
    #[test]
    fn account_windows_are_distinct_from_session_and_billing_period_tokens() {
        let native = json!({"kind":"usage","source":"chatgpt","status":"ready","fetched_at_ms":100,"totals":{"total_tokens":99999},"codex":{"plan":"test","spend_limit_reached":false,"limits":[{"name":"native","reached":false,"primary":{"used_percent":25,"window_seconds":18000,"resets_at_ms":1234}}]}});
        let limits = account_limits(&native);
        assert_eq!(limits.status, "available");
        assert_eq!(limits.limits[0].windows[0].remaining, Some(75.0));
        assert_eq!(
            limits.limits[0].windows[0].window_duration_minutes,
            Some(300)
        );
        assert_eq!(limits.limits[0].windows[0].resets_at_ms, Some(1234));
        assert_eq!(
            account_limits(&json!({"kind":"usage","source":"chatgpt","status":"unavailable"}))
                .status,
            "unavailable"
        );
    }
}
