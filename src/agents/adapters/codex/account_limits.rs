//! Read only, allowlisted projection of official account/rateLimits/read.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::CodexClient;
use crate::agents::domain::account_limits::{
    AccountLimit, AccountLimitWindow, AccountLimits, Credits, IndividualLimit, ResetCredit,
    ResetCredits,
};

#[derive(Default)]
pub(super) struct Cache {
    epoch: AtomicU64,
    value: Mutex<Option<(Instant, u64, AccountLimits)>>,
}

impl Cache {
    pub fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub async fn read(&self, client: &CodexClient) -> AccountLimits {
        if !client.is_ready() {
            return AccountLimits::unavailable("temporarily_unavailable");
        }
        // Single flight across paired devices. No polling or disk persistence.
        let mut cache = self.value.lock().await;
        let epoch = self.epoch.load(Ordering::Acquire);
        if let Some((at, cached_epoch, value)) = cache.as_ref() {
            if *cached_epoch == epoch && at.elapsed() < Duration::from_secs(30) {
                return value.clone();
            }
        }
        let value = match client.call("account/rateLimits/read", json!({})).await {
            Ok(value) => project(&value),
            Err(_) => {
                let account = client
                    .call("account/read", json!({"refreshToken": false}))
                    .await;
                let signed_out = account
                    .as_ref()
                    .is_ok_and(|v| v.get("account") == Some(&Value::Null));
                AccountLimits::unavailable(if signed_out {
                    "not_authenticated"
                } else {
                    "temporarily_unavailable"
                })
            }
        };
        if self.epoch.load(Ordering::Acquire) != epoch {
            return AccountLimits::unavailable("temporarily_unavailable");
        }
        *cache = Some((Instant::now(), epoch, value.clone()));
        value
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(|s| s.chars().take(256).collect())
}

fn timestamp(value: &Value) -> Option<u64> {
    value
        .as_u64()?
        .checked_mul(1000)
        .filter(|v| *v <= 8_640_000_000_000_000)
}

/// Validate a bounded decimal without converting or rounding its value.
fn decimal(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if raw.len() > 256 {
        return None;
    }
    let unsigned = raw.strip_prefix('-').unwrap_or(raw);
    let mut dot = false;
    let mut digits = 0;
    for byte in unsigned.bytes() {
        if byte.is_ascii_digit() {
            digits += 1;
        } else if byte == b'.' && !dot {
            dot = true;
        } else {
            return None;
        }
    }
    (digits > 0).then(|| raw.to_owned())
}

fn project(value: &Value) -> AccountLimits {
    let buckets: Vec<(&str, &Value)> = match value["rateLimitsByLimitId"].as_object() {
        Some(map) if !map.is_empty() => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        _ if value["rateLimits"].is_object() => vec![("default", &value["rateLimits"])],
        _ => return AccountLimits::unavailable("temporarily_unavailable"),
    };
    let limits = buckets
        .into_iter()
        .take(64)
        .filter_map(|(key, bucket)| {
            bucket.as_object()?;
            let id = bucket["limitId"]
                .as_str()
                .unwrap_or(key)
                .chars()
                .take(256)
                .collect();
            let name = bucket["limitName"]
                .as_str()
                .map(|s| s.chars().take(256).collect());
            let windows = ["primary", "secondary"]
                .into_iter()
                .filter_map(|id| {
                    let w = bucket.get(id)?.as_object()?;
                    let used = w
                        .get("usedPercent")
                        .and_then(Value::as_f64)
                        .filter(|v| v.is_finite() && *v >= 0.0);
                    Some(AccountLimitWindow {
                        id: id.into(),
                        label: None,
                        unit: "percent".into(),
                        // Preserve valid upstream overage; never infer remaining balance.
                        used,
                        limit: Some(100.0),
                        remaining: used.map(|v| (100.0 - v).clamp(0.0, 100.0)),
                        window_duration_minutes: w
                            .get("windowDurationMins")
                            .and_then(Value::as_u64)
                            .filter(|v| *v > 0),
                        resets_at_ms: w
                            .get("resetsAt")
                            .and_then(Value::as_u64)
                            .and_then(|v| v.checked_mul(1000))
                            .filter(|v| *v <= 8_640_000_000_000_000),
                    })
                })
                .collect();
            let credits = bucket["credits"].as_object().map(|_| Credits {
                has_credits: bucket["credits"]["hasCredits"].as_bool(),
                unlimited: bucket["credits"]["unlimited"].as_bool(),
                balance: decimal(&bucket["credits"]["balance"]),
            });
            let individual_limit = bucket["individualLimit"]
                .as_object()
                .map(|_| IndividualLimit {
                    used: decimal(&bucket["individualLimit"]["used"]),
                    limit: decimal(&bucket["individualLimit"]["limit"]),
                    remaining_percent: bucket["individualLimit"]["remainingPercent"]
                        .as_f64()
                        .filter(|v| v.is_finite())
                        .map(|v| v.clamp(0.0, 100.0)),
                    resets_at_ms: timestamp(&bucket["individualLimit"]["resetsAt"]),
                });
            Some(AccountLimit {
                id,
                name,
                windows,
                credits,
                individual_limit,
                plan_type: text(&bucket["planType"]),
                model_id: text(&bucket["normalModelSlug"]),
                spend_control_reached: bucket["spendControlReached"].as_bool(),
                limit_reached_reason: text(&bucket["rateLimitReachedType"]),
            })
        })
        .collect::<Vec<_>>();
    if limits.is_empty() {
        return AccountLimits::unavailable("temporarily_unavailable");
    }
    AccountLimits {
        status: "available",
        reason: None,
        observed_at_ms: crate::tasks::domain::now_ms(),
        ordinary_usage_allowed: value["ordinaryUsageAllowed"].as_bool(),
        reset_credits: value["rateLimitResetCredits"]
            .as_object()
            .map(|_| ResetCredits {
                available_count: value["rateLimitResetCredits"]["availableCount"]
                    .as_u64()
                    .filter(|v| *v <= 9_007_199_254_740_991),
                credits: value["rateLimitResetCredits"]["credits"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .take(64)
                            .map(|row| ResetCredit {
                                status: text(&row["status"]),
                                reset_type: text(&row["resetType"]),
                                expires_at_ms: timestamp(&row["expiresAt"]),
                                title: text(&row["title"]),
                            })
                            .collect()
                    }),
            }),
        limits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_only_and_monthly_limits_preserve_official_values_without_inventing_windows() {
        let value = project(&json!({"rateLimits": {
            "credits":{"hasCredits":true,"unlimited":false,"balance":"9007199254740993.125"},
            "individualLimit":{"used":"1.2500","limit":"10000.0001","remainingPercent":79,"resetsAt":1800000000},
            "planType":"business","normalModelSlug":"model-slug",
            "spendControlReached":true,"rateLimitReachedType":"workspace_member_usage_limit_reached"
        }}));
        assert_eq!(value.status, "available");
        assert_eq!(value.ordinary_usage_allowed, None);
        let bucket = &value.limits[0];
        assert!(bucket.windows.is_empty());
        assert_eq!(
            bucket.credits.as_ref().unwrap().balance.as_deref(),
            Some("9007199254740993.125")
        );
        let monthly = bucket.individual_limit.as_ref().unwrap();
        assert_eq!(monthly.used.as_deref(), Some("1.2500"));
        assert_eq!(monthly.limit.as_deref(), Some("10000.0001"));
        assert_eq!(monthly.remaining_percent, Some(79.0));
        assert_eq!(monthly.resets_at_ms, Some(1800000000000));
        assert_eq!(bucket.spend_control_reached, Some(true));
        assert_eq!(bucket.plan_type.as_deref(), Some("business"));
        assert_eq!(bucket.model_id.as_deref(), Some("model-slug"));
        assert_eq!(
            bucket.limit_reached_reason.as_deref(),
            Some("workspace_member_usage_limit_reached")
        );

        let unknown = project(
            &json!({"rateLimits":{"credits":{"hasCredits":true,"unlimited":false,"balance":null},"individualLimit":{}}}),
        );
        let credits = unknown.limits[0].credits.as_ref().unwrap();
        assert_eq!(credits.has_credits, Some(true));
        assert_eq!(credits.balance, None);
        assert_eq!(
            unknown.limits[0]
                .individual_limit
                .as_ref()
                .unwrap()
                .remaining_percent,
            None
        );
        let unlimited = project(&json!({"rateLimits":{"credits":{"unlimited":true}}}));
        assert_eq!(
            unlimited.limits[0].credits.as_ref().unwrap().unlimited,
            Some(true)
        );
        assert_eq!(
            unlimited.limits[0].credits.as_ref().unwrap().has_credits,
            None
        );
    }

    #[test]
    fn reset_counts_keep_unknown_empty_and_capped_details_distinct_and_hide_action_ids() {
        for details in [
            Value::Null,
            json!([]),
            json!([{"id":"private-action-id","status":"available","resetType":"codexRateLimits","expiresAt":1800000000,"title":"Reset"}]),
        ] {
            let result = project(
                &json!({"rateLimits":{},"rateLimitResetCredits":{"availableCount":5,"credits":details}}),
            );
            let resets = result.reset_credits.as_ref().unwrap();
            assert_eq!(resets.available_count, Some(5));
            assert_eq!(
                resets.credits.as_ref().map(Vec::len),
                details.as_array().map(Vec::len)
            );
            if let Some(row) = resets.credits.as_ref().and_then(|v| v.first()) {
                assert_eq!(row.expires_at_ms, Some(1800000000000));
                assert_eq!(row.status.as_deref(), Some("available"));
            }
            assert!(!serde_json::to_string(&result)
                .unwrap()
                .contains("private-action-id"));
        }
        let absent = project(&json!({"rateLimits":{}}));
        assert!(absent.reset_credits.is_none());
    }

    #[tokio::test]
    async fn account_change_during_read_discards_the_old_snapshot() {
        let cache = std::sync::Arc::new(Cache::default());
        let invalidator = cache.clone();
        let mut fixture = super::super::tests::Fixture::new(move |_| {
            invalidator.invalidate();
            json!({"rateLimits":{"primary":{"usedPercent":25}}})
        })
        .await;
        fixture.ready().await;
        assert_eq!(cache.read(&fixture.client).await.status, "unavailable");
    }

    #[test]
    fn projects_multiple_windows_without_identity_credits_or_permission_inference() {
        let value = project(&json!({"accountId":"private", "rateLimitsByLimitId": {
            "codex": {"primary":{"usedPercent":110,"windowDurationMins":300,"resetsAt":1800000000},
                "secondary":{"usedPercent":null,"resetsAt":-1},"credits":{"balance":"secret"}},
            "review": {"primary":{"usedPercent":0,"windowDurationMins":0,"resetsAt":18446744073709551615u64}}
        }}));
        assert_eq!(value.status, "available");
        assert_eq!(value.ordinary_usage_allowed, None);
        assert_eq!(value.limits.len(), 2);
        assert_eq!(value.limits[0].windows[0].used, Some(110.0));
        assert_eq!(value.limits[0].windows[0].remaining, Some(0.0));
        assert_eq!(value.limits[0].windows[0].resets_at_ms, Some(1800000000000));
        assert_eq!(value.limits[0].windows[1].used, None);
        assert_eq!(value.limits[0].windows[1].remaining, None);
        assert_eq!(value.limits[0].windows[1].resets_at_ms, None);
        assert_eq!(value.limits[1].windows[0].resets_at_ms, None);
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(!serialized.contains("private") && !serialized.contains("secret"));
    }

    #[test]
    fn legacy_empty_and_unknown_snapshots_stay_distinct() {
        let legacy = project(
            &json!({"rateLimits":{"primary":null,"secondary":null},"ordinaryUsageAllowed":false}),
        );
        assert_eq!(legacy.status, "available");
        assert!(legacy.limits[0].windows.is_empty());
        assert_eq!(legacy.ordinary_usage_allowed, Some(false));
        assert_eq!(project(&json!({"future":true})).status, "unavailable");
        let weekly_only = project(&json!({"rateLimits":{"primary":{
            "usedPercent":-1,"windowDurationMins":10080
        }}}));
        assert_eq!(weekly_only.limits[0].windows.len(), 1);
        assert_eq!(
            weekly_only.limits[0].windows[0].window_duration_minutes,
            Some(10080)
        );
        assert_eq!(weekly_only.limits[0].windows[0].remaining, None);
    }
}
