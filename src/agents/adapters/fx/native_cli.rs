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
