//! The context-usage readout beside the selector row: "12.4k / 200k · $0.42",
//! from what the session reports (the App's `agentUsageSummary`). Omitted
//! when nothing is known.

use muqun_gateway::agents::{AgentSessionInfo, TokensUsage};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    /// The mono readout.
    pub label: String,
    /// The tooltip's breakdown, one `name: value` per line.
    pub lines: Vec<String>,
}

/// `12400` → `12.4k`, `1_500_000` → `1.5M`.
pub fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn measured_total(tokens: &TokensUsage) -> u64 {
    tokens.input
        + tokens.output
        + tokens.reasoning.unwrap_or(0)
        + tokens.cache_read.unwrap_or(0)
        + tokens.cache_write.unwrap_or(0)
}

fn context_limit(value: Option<&Value>) -> Option<u64> {
    let limit = value?.get("context")?.as_u64()?;
    (limit > 0).then_some(limit)
}

/// The readout for `info`; `catalog_limit` is the selected model's context
/// size when the catalogue knows it.
pub fn summary(info: &AgentSessionInfo, catalog_limit: Option<u64>) -> Option<Usage> {
    let context = info.context_usage.as_ref();
    let current = context.map(|c| {
        if c.total_tokens > 0 {
            c.total_tokens
        } else {
            c.tokens.as_ref().map(measured_total).unwrap_or(0)
        }
    });
    let cost = match info.cost {
        None => None,
        Some(0.0) => Some("Free".to_string()),
        Some(c) => Some(format!("${c:.2}")),
    };
    let (amount, tokens_for_lines, session) = match current.filter(|n| *n > 0) {
        Some(total) => {
            let limit = context
                .and_then(|c| context_limit(c.limit.as_ref()))
                .or_else(|| context_limit(info.limit.as_ref()))
                .or(catalog_limit);
            let amount = match limit {
                Some(limit) => format!("{} / {}", compact(total), compact(limit)),
                None => compact(total),
            };
            (amount, context.and_then(|c| c.tokens.clone()), false)
        }
        None => {
            let spent = info.tokens.as_ref().map(measured_total).unwrap_or(0);
            if spent == 0 {
                return None;
            }
            (compact(spent), info.tokens.clone(), true)
        }
    };
    let label = match &cost {
        Some(cost) => format!("{amount} · {cost}"),
        None => amount,
    };
    let mut lines = vec![format!(
        "{}: {}",
        if session { "Session tokens" } else { "Context" },
        label
    )];
    if let Some(t) = tokens_for_lines {
        lines.push(format!("Input: {}", compact(t.input)));
        lines.push(format!("Output: {}", compact(t.output)));
        for (name, value) in [
            ("Reasoning", t.reasoning),
            ("Cache read", t.cache_read),
            ("Cache write", t.cache_write),
        ] {
            if let Some(v) = value.filter(|v| *v > 0) {
                lines.push(format!("{name}: {}", compact(v)));
            }
        }
    }
    if let Some(cost) = cost {
        lines.push(format!("Cost: {cost}"));
    }
    Some(Usage { label, lines })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(v: Value) -> AgentSessionInfo {
        let mut base = json!({"asid": "a", "title": "t", "status": "idle", "updated_ms": 1});
        base.as_object_mut()
            .unwrap()
            .extend(v.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(12_400), "12.4k");
        assert_eq!(compact(200_000), "200.0k");
        assert_eq!(compact(1_500_000), "1.5M");
    }

    #[test]
    fn context_with_a_limit_and_a_cost() {
        let usage = summary(
            &info(json!({
                "context_usage": {"total_tokens": 12400, "limit": {"context": 200000},
                    "tokens": {"input": 12000, "output": 400}},
                "cost": 0.42
            })),
            None,
        )
        .unwrap();
        assert_eq!(usage.label, "12.4k / 200.0k · $0.42");
        assert!(usage.lines.contains(&"Input: 12.0k".to_string()));
    }

    #[test]
    fn the_catalogue_limit_fills_in_and_free_says_free() {
        let usage = summary(
            &info(json!({"context_usage": {"total_tokens": 5000}, "cost": 0.0})),
            Some(128_000),
        )
        .unwrap();
        assert_eq!(usage.label, "5.0k / 128.0k · Free");
    }

    #[test]
    fn session_spend_stands_in_without_context_and_nothing_is_nothing() {
        let usage = summary(&info(json!({"tokens": {"input": 100, "output": 50}})), None).unwrap();
        assert_eq!(usage.label, "150");
        assert!(usage.lines[0].starts_with("Session tokens"));
        assert!(summary(&info(json!({})), None).is_none());
        assert!(summary(&info(json!({"cost": 1.0})), None).is_none());
    }
}
