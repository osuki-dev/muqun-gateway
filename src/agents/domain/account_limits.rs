//! Optional provider-account rate windows, unrelated to context tokens or billing.
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct AccountLimits {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub observed_at_ms: u64,
    pub ordinary_usage_allowed: Option<bool>,
    pub reset_credits: Option<ResetCredits>,
    pub limits: Vec<AccountLimit>,
}

impl AccountLimits {
    pub fn unavailable(reason: &'static str) -> Self {
        Self {
            status: "unavailable",
            reason: Some(reason),
            observed_at_ms: crate::tasks::domain::now_ms(),
            ordinary_usage_allowed: None,
            reset_credits: None,
            limits: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountLimit {
    pub id: String,
    pub name: Option<String>,
    pub plan_type: Option<String>,
    pub model_id: Option<String>,
    pub credits: Option<Credits>,
    pub individual_limit: Option<IndividualLimit>,
    pub spend_control_reached: Option<bool>,
    pub limit_reached_reason: Option<String>,
    pub windows: Vec<AccountLimitWindow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IndividualLimit {
    pub used: Option<String>,
    pub limit: Option<String>,
    pub remaining_percent: Option<f64>,
    pub resets_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResetCredits {
    pub available_count: Option<u64>,
    pub credits: Option<Vec<ResetCredit>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResetCredit {
    pub status: Option<String>,
    pub reset_type: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AccountLimitWindow {
    pub id: String,
    pub label: Option<String>,
    pub unit: String,
    pub used: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
    pub window_duration_minutes: Option<u64>,
    pub resets_at_ms: Option<u64>,
}
