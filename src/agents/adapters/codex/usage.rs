//! Codex counts cached input and reasoning output inside input/output totals.
//! Neutral counters are disjoint. Invalid/incomplete breakdowns stay unknown.
use serde_json::{json, Value};

use crate::agents::domain::{AgentSessionInfo, SessionContextUsage, TokensUsage};

#[derive(Clone)]
pub(crate) struct Usage {
    total: Option<TokensUsage>,
    context: Option<SessionContextUsage>,
}

pub(crate) fn tokens(value: &Value) -> Option<TokensUsage> {
    let input = value["inputTokens"].as_u64()?;
    let output = value["outputTokens"].as_u64()?;
    let cached = value["cachedInputTokens"].as_u64()?;
    let written = value["cacheWriteInputTokens"].as_u64()?;
    let reasoning = value["reasoningOutputTokens"].as_u64()?;
    Some(TokensUsage {
        input: input.checked_sub(cached)?.checked_sub(written)?,
        output: output.checked_sub(reasoning)?,
        reasoning: Some(reasoning),
        cache_read: Some(cached),
        cache_write: Some(written),
    })
}

impl Usage {
    pub(crate) fn from_wire(value: &Value) -> Self {
        Self {
            total: tokens(&value["total"]),
            context: value["last"]["totalTokens"].as_u64().map(|total_tokens| {
                SessionContextUsage {
                    tokens: tokens(&value["last"]),
                    total_tokens,
                    limit: value["modelContextWindow"]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .map(|n| json!({"context": n})),
                }
            }),
        }
    }

    pub(crate) fn apply(&self, info: &mut AgentSessionInfo) {
        info.tokens.clone_from(&self.total);
        info.context_usage.clone_from(&self.context);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disjoint_counters_preserve_upstream_total() {
        let wire = json!({"inputTokens":100,"cachedInputTokens":40,"cacheWriteInputTokens":60,"outputTokens":10,"reasoningOutputTokens":5,"totalTokens":110});
        let usage = tokens(&wire).unwrap();
        assert_eq!((usage.input, usage.output), (0, 5));
        assert_eq!(
            usage.input
                + usage.output
                + usage.reasoning.unwrap()
                + usage.cache_read.unwrap()
                + usage.cache_write.unwrap(),
            110
        );
        let mapped = Usage::from_wire(
            &json!({"total":wire,"last":{"totalTokens":123},"modelContextWindow":1000}),
        );
        let context = mapped.context.unwrap();
        assert_eq!(context.total_tokens, 123);
        assert_eq!(context.tokens, None);
        assert_eq!(context.limit, Some(json!({"context":1000})));
    }
    #[test]
    fn inconsistent_breakdowns_and_unknown_limits_are_not_zero_filled() {
        assert!(tokens(&json!({"inputTokens":1,"cachedInputTokens":2,"cacheWriteInputTokens":0,"outputTokens":10,"reasoningOutputTokens":5})).is_none());
        assert!(tokens(&json!({"inputTokens":1,"outputTokens":2})).is_none());
        let usage = Usage::from_wire(&json!({"last":{"totalTokens":0},"modelContextWindow":null}));
        assert_eq!(usage.context.unwrap().limit, None);
        assert!(usage.total.is_none());
    }
}
