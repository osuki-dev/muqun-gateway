//! Signal-Cascade System 1 Router & Sticky-Parent Advisor helpers
//! (`agent_orchestration_router_v1`).
//!
//! Design principles:
//! 1. **Sticky Parent Cache Preservation**: When an existing session has a warm
//!    prompt cache (active within 5 min) and healthy quota, prefer `stay` to avoid
//!    the cold-prefill cache-miss penalty.
//! 2. **Zero-Token Hard Signals First (Layer 1)**: Live `account-limits` quota
//!    filtering, connectivity checks, warm-cache affinity, and trivial-query
//!    fast-path (`skip_review = true`) run locally in <1ms with 0 LLM tokens.
//! 3. **Pluggable Optional System 1 (Layer 2, strictly Fail-Open)**: Optionally
//!    calls TypeSafe Jev (`/zen/v1/systemone`) or an OpenAI-compatible fast local
//!    router (e.g. Ollama `Arch-Router-1.5B` / `Qwen3-0.6B`) on fresh turns with a
//!    strict timeout (default 1500ms); any failure silently falls back to Layer 1.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::routes::{check_candidate_quota, is_candidate_exhausted};
use super::{candidate_display, candidate_key, concise_excerpt, ReviewerSpec, Start};
use crate::AppState;

/// Sessions active within this window are treated as having a warm prompt cache.
pub const CACHE_WARM_WINDOW_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateStatus {
    pub spec: ReviewerSpec,
    /// `"healthy"`, `"exhausted"`, or `"unavailable"`
    pub health: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDecision {
    /// `"signal_cascade"`, `"jev"`, or `"openai_compatible"`
    pub engine: String,
    /// `"stay"`, `"switch"`, or `"direct_fast"`
    pub action: String,
    pub cache_warm: bool,
    pub skip_review: bool,
    pub chosen_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen_model: Option<String>,
    pub reason: String,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<CandidateStatus>,
}

/// Optional external System 1 router configuration (defaults to zero-config `"builtin"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalRouterConfig {
    /// `"builtin"` (default), `"jev"`, or `"openai_compatible"` (e.g. Ollama Arch-Router-1.5B)
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Natural-language preference policy (Arch-Router / Jev style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(default = "ExternalRouterConfig::default_timeout_ms")]
    pub timeout_ms: u64,
}

impl ExternalRouterConfig {
    const fn default_timeout_ms() -> u64 {
        1_500
    }

    pub fn from_env() -> Self {
        let provider = std::env::var("MUQUN_MAO_ROUTER_PROVIDER")
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "builtin".into());
        Self {
            provider,
            endpoint: std::env::var("MUQUN_MAO_ROUTER_ENDPOINT")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            api_key: std::env::var("MUQUN_MAO_ROUTER_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            model: std::env::var("MUQUN_MAO_ROUTER_MODEL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            policy: std::env::var("MUQUN_MAO_ROUTER_POLICY")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            timeout_ms: std::env::var("MUQUN_MAO_ROUTER_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|&ms| (200..=10_000).contains(&ms))
                .unwrap_or_else(Self::default_timeout_ms),
        }
    }
}

impl Default for ExternalRouterConfig {
    fn default() -> Self {
        Self {
            provider: "builtin".into(),
            endpoint: None,
            api_key: None,
            model: None,
            policy: None,
            timeout_ms: Self::default_timeout_ms(),
        }
    }
}

/// Detect clearly lightweight conversational / read-only prompts so we avoid
/// burning extra review tokens on pure questions.
pub fn is_lightweight_query(goal: &str) -> bool {
    let trimmed = goal.trim();
    if trimmed.is_empty() || trimmed.len() > 260 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    const EDIT_VERBS: &[&str] = &[
        "fix",
        "implement",
        "refactor",
        "add ",
        "create ",
        "delete ",
        "remove ",
        "update ",
        "change ",
        "edit ",
        "build ",
        "write ",
        "修复",
        "修改",
        "重构",
        "实现",
        "新增",
        "添加",
        "加上",
        "删除",
        "优化",
        "改造",
        "写一个",
        "帮我改",
        "帮我修",
        "帮我做",
        "做出来",
    ];
    if EDIT_VERBS
        .iter()
        .any(|v| lower.contains(v) || trimmed.contains(v))
    {
        return false;
    }
    if trimmed.ends_with('?') || trimmed.ends_with('？') {
        return true;
    }
    const QUERY_PREFIXES: &[&str] = &[
        "what ",
        "why ",
        "how ",
        "where ",
        "which ",
        "explain ",
        "summarize ",
        "describe ",
        "为什么",
        "怎么",
        "怎样",
        "在哪",
        "哪里",
        "是什么",
        "有什么",
        "分析一下",
        "研究一下",
        "解释",
        "总结",
    ];
    QUERY_PREFIXES
        .iter()
        .any(|p| lower.starts_with(p) || trimmed.starts_with(p))
}

/// Probe live health and real-time account quotas for the user's configured pool,
/// automatically discovering connected agents when `agent_pool` is empty.
pub(super) async fn probe_pool(
    state: &AppState,
    request: &Start,
    require_mcp: bool,
) -> Vec<CandidateStatus> {
    let mut specs: Vec<ReviewerSpec> = Vec::new();
    let mut push_unique = |spec: ReviewerSpec| {
        let key = candidate_key(&spec);
        if !specs.iter().any(|s| candidate_key(s) == key) {
            specs.push(spec);
        }
    };

    for item in &request.agent_pool {
        if !item.agent.trim().is_empty() {
            push_unique(item.clone());
        }
    }
    push_unique(ReviewerSpec {
        agent: request.coordinator.clone(),
        model: request.model.clone(),
        variant: request.variant.clone(),
        machine: request.machine.clone(),
    });
    if let Some(ref rev) = request.reviewer {
        push_unique(rev.clone());
    }
    if request.agent_pool.is_empty() {
        for mgr in state.agent_runtime.all_managers().await {
            push_unique(ReviewerSpec {
                agent: mgr.agent().kind().to_string(),
                model: None,
                variant: None,
                machine: None,
            });
        }
    }

    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        match check_candidate_quota(state, &spec, require_mcp).await {
            Ok(()) => out.push(CandidateStatus {
                spec,
                health: "healthy".into(),
                reason: None,
            }),
            Err((reason, _)) => {
                let health = if reason.contains("not connected") || reason.contains("no Muqun MCP")
                {
                    "unavailable"
                } else {
                    "exhausted"
                };
                out.push(CandidateStatus {
                    spec,
                    health: health.into(),
                    reason: Some(reason),
                });
            }
        }
    }
    out
}

/// Run the 2-layer Signal Cascade (Hard Signals -> Optional Fail-Open System 1).
pub(super) async fn decide_route(
    state: &AppState,
    request: &Start,
    primary: &ReviewerSpec,
    session_updated_ms: Option<u64>,
    now_ms: u64,
    exhausted: &[String],
    require_mcp: bool,
) -> (ReviewerSpec, RouteDecision) {
    let started = Instant::now();
    let candidates = probe_pool(state, request, require_mcp).await;
    let healthy: Vec<&CandidateStatus> = candidates
        .iter()
        .filter(|c| c.health == "healthy" && !is_candidate_exhausted(exhausted, &c.spec))
        .collect();

    let cache_warm = request.asid.is_some()
        && session_updated_ms
            .is_some_and(|ts| ts > 0 && now_ms.saturating_sub(ts) <= CACHE_WARM_WINDOW_MS);
    let lightweight = is_lightweight_query(&request.goal);

    let primary_status = candidates
        .iter()
        .find(|c| candidate_key(&c.spec) == candidate_key(primary))
        .or_else(|| {
            candidates
                .iter()
                .find(|c| c.spec.agent.eq_ignore_ascii_case(&primary.agent))
        });
    let primary_healthy = primary_status.is_some_and(|c| c.health == "healthy")
        && !is_candidate_exhausted(exhausted, primary);

    // 1. If primary is healthy and prompt cache is warm -> LOCK sticky parent!
    if primary_healthy && cache_warm {
        let reason = if lightweight {
            format!(
                "Warm session cache on {} · lightweight query (skip review)",
                primary.agent
            )
        } else {
            format!(
                "Sticky parent cache warm on {} · avoiding cold-cache switch",
                primary.agent
            )
        };
        return (
            primary.clone(),
            RouteDecision {
                engine: "signal_cascade".into(),
                action: if lightweight { "direct_fast" } else { "stay" }.into(),
                cache_warm: true,
                skip_review: lightweight,
                chosen_agent: primary.agent.clone(),
                chosen_model: primary.model.clone(),
                reason,
                latency_ms: started.elapsed().as_millis() as u64,
                candidates,
            },
        );
    }

    // 2. If primary is healthy and fresh session, optionally consult pluggable System 1 router
    // (Jev or local OpenAI-compatible Arch-Router) when configured and >=2 healthy candidates exist.
    let ext_cfg = ExternalRouterConfig::from_env();
    if ext_cfg.provider != "builtin" && healthy.len() >= 2 && !cache_warm {
        if let Some((picked, reason, skip_rev)) =
            consult_external_router(&ext_cfg, &request.goal, &healthy).await
        {
            let switched =
                !picked.agent.eq_ignore_ascii_case(&primary.agent) || picked.model != primary.model;
            return (
                picked.clone(),
                RouteDecision {
                    engine: ext_cfg.provider,
                    action: if skip_rev {
                        "direct_fast"
                    } else if switched {
                        "switch"
                    } else {
                        "stay"
                    }
                    .into(),
                    cache_warm: false,
                    skip_review: skip_rev || lightweight,
                    chosen_agent: picked.agent,
                    chosen_model: picked.model,
                    reason,
                    latency_ms: started.elapsed().as_millis() as u64,
                    candidates,
                },
            );
        }
    }

    // 3. Primary healthy -> stay on primary with fast-path flag for lightweight queries.
    if primary_healthy {
        let reason = if lightweight {
            format!(
                "Lightweight query on {} · fast path (skip review)",
                primary.agent
            )
        } else {
            format!("Primary {} healthy with available quota", primary.agent)
        };
        return (
            primary.clone(),
            RouteDecision {
                engine: "signal_cascade".into(),
                action: if lightweight { "direct_fast" } else { "stay" }.into(),
                cache_warm,
                skip_review: lightweight,
                chosen_agent: primary.agent.clone(),
                chosen_model: primary.model.clone(),
                reason,
                latency_ms: started.elapsed().as_millis() as u64,
                candidates,
            },
        );
    }

    // 4. Primary exhausted/unavailable -> route to first healthy candidate in pool.
    let auto_fo = request
        .auto_failover
        .unwrap_or(!request.agent_pool.is_empty());
    if auto_fo {
        if let Some(next) = healthy.first() {
            let err_reason = primary_status
                .and_then(|s| s.reason.as_deref())
                .unwrap_or("quota exhausted");
            let reason = format!(
                "{} unavailable ({err_reason}) → routed to {}",
                candidate_display(primary),
                candidate_display(&next.spec)
            );
            return (
                next.spec.clone(),
                RouteDecision {
                    engine: "signal_cascade".into(),
                    action: "switch".into(),
                    cache_warm: false,
                    skip_review: lightweight,
                    chosen_agent: next.spec.agent.clone(),
                    chosen_model: next.spec.model.clone(),
                    reason,
                    latency_ms: started.elapsed().as_millis() as u64,
                    candidates,
                },
            );
        }
    }

    (
        primary.clone(),
        RouteDecision {
            engine: "signal_cascade".into(),
            action: "stay".into(),
            cache_warm,
            skip_review: lightweight,
            chosen_agent: primary.agent.clone(),
            chosen_model: primary.model.clone(),
            reason: "Proceeding with requested agent".into(),
            latency_ms: started.elapsed().as_millis() as u64,
            candidates,
        },
    )
}

/// Fail-open external System 1 router supporting TypeSafe Jev or any OpenAI-compatible
/// local fast router (e.g. Ollama `Arch-Router-1.5B` / `Qwen3-0.6B`).
async fn consult_external_router(
    cfg: &ExternalRouterConfig,
    goal: &str,
    healthy: &[&CandidateStatus],
) -> Option<(ReviewerSpec, String, bool)> {
    let endpoint = cfg.endpoint.as_deref()?.trim();
    if endpoint.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .build()
        .ok()?;

    let choices: Vec<String> = healthy.iter().map(|c| candidate_display(&c.spec)).collect();
    let prompt_snippet = concise_excerpt(goal, 600);
    let policy = cfg
        .policy
        .as_deref()
        .unwrap_or("Prefer fast/cheap agents for simple tweaks and top reasoning agents for complex architecture or debugging.");

    if cfg.provider == "jev" {
        // TypeSafe Jev System One structured choice request (zero prose).
        let mut req = client.post(endpoint).json(&json!({
            "context": format!("Policy: {policy}\nUser task: {prompt_snippet}"),
            "questions": [
                {
                    "id": "candidate",
                    "type": "choice",
                    "prompt": "Best agent/model for this task",
                    "choices": choices,
                },
                {
                    "id": "skip_review",
                    "type": "bool",
                    "prompt": "Is this a simple Q&A or trivial tweak that does not need independent code review?"
                }
            ]
        }));
        if let Some(ref key) = cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        let picked_str = body
            .pointer("/answers/candidate/value")
            .or_else(|| body.pointer("/results/candidate"))
            .and_then(|v| v.as_str())?;
        let skip_rev = body
            .pointer("/answers/skip_review/value")
            .or_else(|| body.pointer("/results/skip_review"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let chosen = healthy
            .iter()
            .find(|c| {
                candidate_display(&c.spec).eq_ignore_ascii_case(picked_str)
                    || c.spec.agent.eq_ignore_ascii_case(picked_str)
            })?
            .spec
            .clone();
        return Some((
            chosen,
            format!("Jev System-1 routed to {picked_str}"),
            skip_rev,
        ));
    }

    if cfg.provider == "openai_compatible" {
        let model = cfg.model.as_deref().unwrap_or("arch-router-1.5b");
        let sys = format!(
            "You are a fast System-1 task router. Policy: {policy}\n\
             Available healthy candidates: {}\n\
             Reply ONLY with compact JSON: {{\"agent\":\"<exact agent>\",\"skip_review\":false,\"reason\":\"<short>\"}}",
            choices.join(" | ")
        );
        let mut req = client.post(endpoint).json(&json!({
            "model": model,
            "temperature": 0.0,
            "max_tokens": 80,
            "messages": [
                { "role": "system", "content": sys },
                { "role": "user", "content": prompt_snippet }
            ]
        }));
        if let Some(ref key) = cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        let content = body
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())?;
        let start = content.find('{')?;
        let end = content.rfind('}')?;
        let parsed: serde_json::Value = serde_json::from_str(&content[start..=end]).ok()?;
        let agent_str = parsed.get("agent").and_then(|v| v.as_str())?;
        let skip_rev = parsed
            .get("skip_review")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let reason = parsed
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|r| concise_excerpt(r, 140))
            .unwrap_or_else(|| format!("Routed by {model}"));
        let chosen = healthy
            .iter()
            .find(|c| {
                c.spec.agent.eq_ignore_ascii_case(agent_str)
                    || candidate_display(&c.spec).eq_ignore_ascii_case(agent_str)
            })?
            .spec
            .clone();
        return Some((chosen, reason, skip_rev));
    }

    None
}

/// Ultra-compact (<150 token) invisible advisor/coordinator guidance injected into
/// the Sticky Parent session so it delegates intelligently without wasting tokens.
pub fn format_advisor_coordinator_instruction(
    candidates: &[CandidateStatus],
    reviewer: Option<&ReviewerSpec>,
) -> String {
    let healthy: Vec<String> = candidates
        .iter()
        .filter(|c| c.health == "healthy")
        .take(8)
        .map(|c| candidate_display(&c.spec))
        .collect();
    if healthy.is_empty() {
        return String::new();
    }
    let exhausted: Vec<String> = candidates
        .iter()
        .filter(|c| c.health == "exhausted")
        .take(4)
        .map(|c| c.spec.agent.clone())
        .collect();
    let skip_note = if exhausted.is_empty() {
        String::new()
    } else {
        format!(" Avoid rate-limited: {}.", exhausted.join(", "))
    };
    let rev_note = reviewer
        .map(|r| format!(" Designated reviewer: {}.", candidate_display(r)))
        .unwrap_or_default();
    format!(
        "Lead Advisor mode — Live healthy agents: [{}].{skip_note}{rev_note} \
         Keep simple/local work in this warm session (0 extra overhead). \
         You decide when to delegate: call `fleet.describe` for full models/quotas across local & linked computers, \
         `tasks.delegate` (or pass `reuse_task_id` to continue a warm child session), \
         and batch `tasks.wait(task_ids=[...])` to collect parallel subtasks in one call \
         (keep disjoint files in the same workspace; only use a git worktree if parallel edits risk conflicting).",
        healthy.join(", ")
    )
}

/// Build a concise consultation prompt for on-demand Advisor escalation
/// (`POST /api/orchestrations/{id}/advisor`) — keeps the Sticky Parent's cache warm.
pub fn build_advisor_prompt(
    goal: &str,
    question: Option<&str>,
    coder_summary: Option<&str>,
    diff_summary: &str,
) -> String {
    let mut parts = Vec::new();
    parts.push(
        "You are a **Read-Only Senior Technical Advisor** consulted by a coding session to unblock a decision or tricky bug.\n\
         STRICT RULES: Do NOT edit files. Be concise (<=12 bullet lines): give root cause, concrete next steps, and edge cases to watch."
            .to_string(),
    );
    parts.push(format!("Task goal:\n{}", concise_excerpt(goal, 800)));
    if let Some(q) = question.map(str::trim).filter(|q| !q.is_empty()) {
        parts.push(format!("Specific question / blocker:\n{q}"));
    }
    if let Some(sum) = coder_summary.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("Current progress:\n{}", concise_excerpt(sum, 600)));
    }
    if !diff_summary.trim().is_empty() {
        parts.push(format!(
            "Current diff / evidence:\n{}",
            concise_excerpt(diff_summary, 3000)
        ));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightweight_query_classifier_distinguishes_qa_from_code_edits() {
        assert!(is_lightweight_query("为什么这里重启后会重置？"));
        assert!(is_lightweight_query("How does session routing work?"));
        assert!(!is_lightweight_query("修复这个重启后重置的 bug"));
        assert!(!is_lightweight_query(
            "Please refactor routes.rs and add tests"
        ));
    }

    #[test]
    fn advisor_coordinator_instruction_stays_compact_and_lists_exhausted_agents() {
        let candidates = vec![
            CandidateStatus {
                spec: ReviewerSpec {
                    agent: "codex".into(),
                    model: None,
                    variant: None,
                    machine: None,
                },
                health: "exhausted".into(),
                reason: Some("rate limit".into()),
            },
            CandidateStatus {
                spec: ReviewerSpec {
                    agent: "cursor".into(),
                    model: None,
                    variant: None,
                    machine: None,
                },
                health: "healthy".into(),
                reason: None,
            },
            CandidateStatus {
                spec: ReviewerSpec {
                    agent: "claude".into(),
                    model: Some("sonnet".into()),
                    variant: None,
                    machine: None,
                },
                health: "healthy".into(),
                reason: None,
            },
        ];
        let text = format_advisor_coordinator_instruction(&candidates, None);
        assert!(text.len() < 700, "must stay compact (<150 tokens): {text}");
        assert!(text.contains("cursor"));
        assert!(text.contains("claude (sonnet)"));
        assert!(text.contains("Avoid rate-limited: codex"));
    }
}
