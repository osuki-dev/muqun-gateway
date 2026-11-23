//! Structured analysis, task decomposition (DAG), and agent assignment protocol
//! for intelligent orchestration (`agent_orchestration_dag_v1`).

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{candidate_display, ReviewerSpec};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateStatus {
    pub spec: ReviewerSpec,
    /// `"healthy"`, `"exhausted"`, or `"unavailable"`
    pub health: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn default_pending() -> String {
    "pending".into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTask {
    pub id: String,
    pub title: String,
    pub goal: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub owns_paths: Vec<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub use_worktree: bool,
    #[serde(default = "default_pending")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PlannedTask {
    pub fn spec(&self) -> ReviewerSpec {
        ReviewerSpec {
            agent: self.agent.clone(),
            model: self.model.clone(),
            variant: self.variant.clone(),
            machine: self.machine.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationPlan {
    /// `"direct"` (L1), `"single"` (L2), or `"parallel_dag"` (L3)
    pub complexity: String,
    /// Concise 1-2 sentence problem analysis
    pub analysis: String,
    pub tasks: Vec<PlannedTask>,
    #[serde(default)]
    pub verify_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ReviewerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct TaskAssignmentOverride {
    pub task_id: String,
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
}

/// Build a compact read-only snapshot of the target workspace so the Planner can
/// reason about structure and repo type quickly.
pub fn workspace_snapshot(cwd: &str) -> String {
    let dir = Path::new(cwd.trim());
    if !dir.is_dir() {
        return format!("Workspace: {cwd} (not a local directory)");
    }
    let mut lines = Vec::new();
    lines.push(format!("Workspace: {}", dir.display()));
    if let Some(repo) = super::worktree::inspect_repo(dir) {
        let wt_note = if repo.is_linked_worktree {
            "linked git worktree"
        } else {
            "git repo"
        };
        let branch = repo.branch.as_deref().unwrap_or("detached");
        let dirty = if repo.was_dirty { "dirty" } else { "clean" };
        lines.push(format!(
            "VCS: {wt_note} (root: {}, branch: {branch}, {dirty})",
            repo.toplevel.display()
        ));
    } else {
        lines.push("VCS: not a git repository".into());
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.to_string();
                if matches!(name.as_str(), ".git" | "target" | "node_modules") {
                    return None;
                }
                let suffix = if e.path().is_dir() { "/" } else { "" };
                Some(format!("{name}{suffix}"))
            })
            .collect();
        names.sort();
        if !names.is_empty() {
            let preview: Vec<String> = names.into_iter().take(30).collect();
            lines.push(format!("Top-level entries: {}", preview.join(", ")));
        }
    }
    lines.join("\n")
}

/// Prompt sent to the read-only Planner agent to analyze the user's request, inspect
/// relevant files if needed, and assign the best healthy agent/model to each task.
pub fn planner_prompt(
    goal: &str,
    snapshot: &str,
    candidates: &[CandidateStatus],
    default_reviewer: Option<&ReviewerSpec>,
    allow_worktrees: bool,
) -> String {
    let mut pool_lines = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        let label = candidate_display(&c.spec);
        let status = match c.health.as_str() {
            "healthy" => "AVAILABLE".to_string(),
            other => match c.reason.as_deref() {
                Some(r) if !r.is_empty() => format!("DO NOT USE ({other}: {r})"),
                _ => format!("DO NOT USE ({other})"),
            },
        };
        pool_lines.push(format!(
            "{}. agent=`{}`{} — {}",
            i + 1,
            c.spec.agent,
            if label != c.spec.agent {
                format!(" ({label})")
            } else {
                String::new()
            },
            status
        ));
    }
    let reviewer_hint = default_reviewer
        .map(candidate_display)
        .unwrap_or_else(|| "auto-select a different healthy agent".into());
    let worktree_rule = if allow_worktrees {
        "Git worktree isolation IS available: when complexity is `parallel_dag`, parallel write tasks touching the same git repo MUST have disjoint `owns_paths` and will run in isolated git worktrees before automatic merge & integration verification. If two tasks must edit the same shared contract/types file, put the shared contract task first and have the parallel tasks `depends_on` it."
    } else {
        "Git worktree isolation is NOT available in this workspace (non-git or disabled): keep write tasks serialized via `depends_on`."
    };

    format!(
        "You are the **Lead Technical Planner & Router** for Muqun Intelligent Orchestration.\n\n\
         STRICT RULES:\n\
         - **READ-ONLY ANALYSIS ONLY**: NEVER edit, write, create, or delete files, and NEVER run mutating commands. You may quickly inspect relevant files/dirs to understand the real scope.\n\
         - Choose agents ONLY from candidates marked `AVAILABLE` below. NEVER pick a `DO NOT USE` candidate.\n\
         - Be pragmatic and honest about complexity — do NOT over-engineer simple tasks:\n\
           1. `\"direct\"` (L1): Pure Q&A, code explanation, investigation, or trivial 1-file tweak. Output 1 task assigned to the fastest/best-suited AVAILABLE agent. No heavyweight review loop will run.\n\
           2. `\"single\"` (L2): Concrete single-module bug fix or bounded feature. Output 1 coding task assigned to the strongest suitable AVAILABLE agent, plus verification command(s) and an independent cross-model reviewer.\n\
           3. `\"parallel_dag\"` (L3): Multi-module / cross-layer feature or refactor (e.g., backend + desktop/UI + tests) that genuinely benefits from 2-4 parallel/staged subtasks with clear `owns_paths` and `depends_on`.\n\
         - {worktree_rule}\n\
         - For EVERY task, write a concise 1-sentence `reason` explaining WHY that specific agent/model was chosen.\n\
         - Preferred reviewer hint: {reviewer_hint} (prefer a DIFFERENT healthy agent from the primary coder when multiple agents are AVAILABLE).\n\n\
         Workspace context:\n{snapshot}\n\n\
         Live Agent & Model Pool:\n{}\n\n\
         User Request:\n{goal}\n\n\
         Output ONLY a short summary followed by a fenced JSON block labeled `MAO_PLAN_JSON` matching this exact schema:\n\
         MAO_PLAN_JSON:\n\
         ```json\n\
         {{\n\
           \"complexity\": \"direct\" | \"single\" | \"parallel_dag\",\n\
           \"analysis\": \"1-2 concise sentences explaining the real problem & scope\",\n\
           \"tasks\": [\n\
             {{\n\
               \"id\": \"t1\",\n\
               \"title\": \"Short task title\",\n\
               \"goal\": \"Clear, actionable, self-contained instructions for this agent\",\n\
               \"agent\": \"<exact AVAILABLE agent id>\",\n\
               \"model\": null,\n\
               \"reason\": \"Why this agent/model fits this task\",\n\
               \"depends_on\": [],\n\
               \"owns_paths\": [\"path/or/dir\"],\n\
               \"read_only\": false\n\
             }}\n\
           ],\n\
           \"verify_commands\": [\"optional verification command\"],\n\
           \"reviewer\": {{\"agent\": \"<AVAILABLE agent id>\", \"model\": null}},\n\
           \"reviewer_reason\": \"Why this reviewer was selected\"\n\
         }}\n\
         ```",
        pool_lines.join("\n")
    )
}

#[derive(Deserialize)]
struct RawPlan {
    #[serde(default)]
    complexity: Option<String>,
    #[serde(default)]
    analysis: Option<String>,
    #[serde(default)]
    tasks: Vec<RawTask>,
    #[serde(default)]
    verify_commands: Vec<String>,
    #[serde(default)]
    reviewer: Option<RawReviewer>,
    #[serde(default)]
    reviewer_reason: Option<String>,
}

#[derive(Deserialize)]
struct RawReviewer {
    agent: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    machine: Option<String>,
}

#[derive(Deserialize)]
struct RawTask {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    goal: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    owns_paths: Vec<String>,
    #[serde(default)]
    read_only: bool,
}

fn extract_json_object(reply: &str) -> Option<&str> {
    // 1. Prefer fenced ```json ... ``` block after MAO_PLAN_JSON (or anywhere).
    let search_from = reply.find("MAO_PLAN_JSON").unwrap_or(0);
    let tail = &reply[search_from..];
    if let Some(fence_start) = tail.find("```") {
        let after_fence = &tail[fence_start + 3..];
        let body_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after_fence[body_start..];
        if let Some(fence_end) = body.find("```") {
            let candidate = body[..fence_end].trim();
            if candidate.starts_with('{') && candidate.ends_with('}') {
                return Some(candidate);
            }
        }
    }
    // 2. Fallback: find outermost `{ ... }` block.
    let start = tail.find('{')?;
    let end = tail.rfind('}')?;
    (end > start).then(|| tail[start..=end].trim())
}

fn match_healthy_candidate(
    requested_agent: Option<&str>,
    requested_model: Option<&str>,
    requested_variant: Option<&str>,
    requested_machine: Option<&str>,
    healthy: &[CandidateStatus],
) -> (ReviewerSpec, bool) {
    if let Some(agent) = requested_agent.map(str::trim).filter(|a| !a.is_empty()) {
        // Exact agent + model match first.
        if let Some(model) = requested_model.map(str::trim).filter(|m| !m.is_empty()) {
            if let Some(found) = healthy.iter().find(|c| {
                c.spec.agent.eq_ignore_ascii_case(agent)
                    && c.spec
                        .model
                        .as_deref()
                        .is_some_and(|m| m.eq_ignore_ascii_case(model))
            }) {
                return (found.spec.clone(), false);
            }
        }
        // Agent match (preserving requested model if agent is healthy).
        if let Some(found) = healthy
            .iter()
            .find(|c| c.spec.agent.eq_ignore_ascii_case(agent))
        {
            let mut spec = found.spec.clone();
            if let Some(m) = requested_model.map(str::trim).filter(|m| !m.is_empty()) {
                spec.model = Some(m.to_string());
            }
            if let Some(v) = requested_variant.map(str::trim).filter(|v| !v.is_empty()) {
                spec.variant = Some(v.to_string());
            }
            if let Some(mc) = requested_machine.map(str::trim).filter(|m| !m.is_empty()) {
                spec.machine = Some(mc.to_string());
            }
            return (spec, false);
        }
    }
    // Fallback to first healthy candidate.
    if let Some(first) = healthy.first() {
        return (first.spec.clone(), true);
    }
    (
        ReviewerSpec {
            agent: requested_agent.unwrap_or("claude").to_string(),
            model: requested_model.map(str::to_string),
            variant: requested_variant.map(str::to_string),
            machine: requested_machine.map(str::to_string),
        },
        true,
    )
}

/// Determine whether two write tasks can run concurrently (neither reaches the other
/// through `depends_on`), which requires git worktree isolation.
pub fn has_parallel_write_tasks(tasks: &[PlannedTask]) -> bool {
    let write_tasks: Vec<&PlannedTask> = tasks.iter().filter(|t| !t.read_only).collect();
    if write_tasks.len() < 2 {
        return false;
    }
    let reaches = |from: &str, target: &str| -> bool {
        let mut stack = vec![from];
        let mut seen = HashSet::new();
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur) {
                continue;
            }
            if let Some(node) = tasks.iter().find(|t| t.id == cur) {
                for dep in &node.depends_on {
                    if dep == target {
                        return true;
                    }
                    stack.push(dep.as_str());
                }
            }
        }
        false
    };
    for i in 0..write_tasks.len() {
        for j in (i + 1)..write_tasks.len() {
            let a = &write_tasks[i].id;
            let b = &write_tasks[j].id;
            if !reaches(a, b) && !reaches(b, a) {
                return true;
            }
        }
    }
    false
}

/// Parse and sanitize a Planner reply into a valid, acyclic, quota-safe `OrchestrationPlan`.
pub fn parse_plan_from_reply(
    reply: &str,
    fallback_goal: &str,
    candidates: &[CandidateStatus],
    default_reviewer: Option<&ReviewerSpec>,
    allow_worktrees: bool,
) -> OrchestrationPlan {
    let healthy: Vec<CandidateStatus> = candidates
        .iter()
        .filter(|c| c.health == "healthy")
        .cloned()
        .collect();
    let pool = if healthy.is_empty() {
        candidates.to_vec()
    } else {
        healthy
    };

    let raw = extract_json_object(reply)
        .and_then(|json_str| serde_json::from_str::<RawPlan>(json_str).ok());

    let Some(raw) = raw else {
        let (spec, _) = match_healthy_candidate(None, None, None, None, &pool);
        let reviewer = pick_reviewer(default_reviewer, None, &spec.agent, &pool);
        return OrchestrationPlan {
            complexity: "single".into(),
            analysis: super::concise_excerpt(
                if reply.trim().is_empty() {
                    "Proceeding with single-agent execution."
                } else {
                    reply.trim()
                },
                240,
            ),
            tasks: vec![PlannedTask {
                id: "t1".into(),
                title: super::concise_excerpt(fallback_goal, 60),
                goal: fallback_goal.to_string(),
                agent: spec.agent,
                model: spec.model,
                variant: spec.variant,
                machine: spec.machine,
                reason: "Assigned to best available healthy agent in pool.".into(),
                depends_on: Vec::new(),
                owns_paths: Vec::new(),
                read_only: false,
                use_worktree: false,
                status: "pending".into(),
                task_id: None,
                asid: None,
                worktree_path: None,
                worktree_branch: None,
                summary: None,
                error: None,
            }],
            verify_commands: Vec::new(),
            reviewer,
            reviewer_reason: Some("Independent verification reviewer.".into()),
        };
    };

    let mut tasks: Vec<PlannedTask> = Vec::new();
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    for (idx, rt) in raw.tasks.into_iter().take(6).enumerate() {
        let mut id = rt
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("t{}", idx + 1));
        while !seen_ids.insert(id.clone()) {
            id.push('_');
        }
        let goal = rt
            .goal
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fallback_goal.to_string());
        let title = rt
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| super::concise_excerpt(&goal, 60));
        let (spec, substituted) = match_healthy_candidate(
            rt.agent.as_deref(),
            rt.model.as_deref(),
            rt.variant.as_deref(),
            rt.machine.as_deref(),
            &pool,
        );
        let mut reason = rt
            .reason
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        if substituted {
            let note = format!("Auto-routed to healthy `{}` from pool.", spec.agent);
            if reason.is_empty() {
                reason = note;
            } else {
                reason = format!("{reason} ({note})");
            }
        } else if reason.is_empty() {
            reason = format!("Matched `{}` for this subtask.", spec.agent);
        }
        tasks.push(PlannedTask {
            id,
            title,
            goal,
            agent: spec.agent,
            model: spec.model,
            variant: spec.variant,
            machine: spec.machine,
            reason,
            depends_on: rt.depends_on,
            owns_paths: rt
                .owns_paths
                .into_iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .take(12)
                .collect(),
            read_only: rt.read_only,
            use_worktree: false,
            status: "pending".into(),
            task_id: None,
            asid: None,
            worktree_path: None,
            worktree_branch: None,
            summary: None,
            error: None,
        });
    }

    if tasks.is_empty() {
        let (spec, _) = match_healthy_candidate(None, None, None, None, &pool);
        tasks.push(PlannedTask {
            id: "t1".into(),
            title: super::concise_excerpt(fallback_goal, 60),
            goal: fallback_goal.to_string(),
            agent: spec.agent,
            model: spec.model,
            variant: spec.variant,
            machine: spec.machine,
            reason: "Assigned to best available healthy agent in pool.".into(),
            depends_on: Vec::new(),
            owns_paths: Vec::new(),
            read_only: false,
            use_worktree: false,
            status: "pending".into(),
            task_id: None,
            asid: None,
            worktree_path: None,
            worktree_branch: None,
            summary: None,
            error: None,
        });
    }

    // Sanitize depends_on to prevent unknown IDs or cycles (topological forward-only).
    let valid_ids: HashSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let mut prior_ids: HashSet<String> = HashSet::new();
    for t in &mut tasks {
        t.depends_on
            .retain(|d| d != &t.id && valid_ids.contains(d) && prior_ids.contains(d));
        prior_ids.insert(t.id.clone());
    }

    let raw_complexity = raw
        .complexity
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let complexity = match raw_complexity.as_str() {
        "direct" | "l1" if tasks.len() == 1 => "direct".to_string(),
        "parallel_dag" | "parallel" | "dag" | "l3" if tasks.len() >= 2 => {
            "parallel_dag".to_string()
        }
        _ if tasks.len() >= 2 => "parallel_dag".to_string(),
        _ => "single".to_string(),
    };

    // Enable git worktree isolation whenever parallel write tasks exist and git worktrees are allowed.
    let parallel_writes = has_parallel_write_tasks(&tasks);
    if parallel_writes {
        if allow_worktrees {
            for t in &mut tasks {
                if !t.read_only {
                    t.use_worktree = true;
                }
            }
        } else {
            // Non-git workspace: serialize write tasks so they don't overwrite each other.
            let mut last_write_id: Option<String> = None;
            for t in &mut tasks {
                if !t.read_only {
                    if let Some(prev) = &last_write_id {
                        if !t.depends_on.contains(prev) {
                            t.depends_on.push(prev.clone());
                        }
                    }
                    last_write_id = Some(t.id.clone());
                }
            }
        }
    }

    let primary_agent = tasks.first().map(|t| t.agent.as_str()).unwrap_or("claude");
    let raw_rev_spec = raw.reviewer.map(|r| ReviewerSpec {
        agent: r.agent,
        model: r.model,
        variant: r.variant,
        machine: r.machine,
    });
    let reviewer = if complexity == "direct" {
        None
    } else {
        pick_reviewer(
            default_reviewer,
            raw_rev_spec.as_ref(),
            primary_agent,
            &pool,
        )
    };

    let analysis = raw
        .analysis
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| super::concise_excerpt(s, 360))
        .unwrap_or_else(|| format!("Planned {} subtask(s) ({complexity}).", tasks.len()));

    OrchestrationPlan {
        complexity,
        analysis,
        tasks,
        verify_commands: raw
            .verify_commands
            .into_iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .take(4)
            .collect(),
        reviewer,
        reviewer_reason: raw
            .reviewer_reason
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

fn pick_reviewer(
    default_reviewer: Option<&ReviewerSpec>,
    planned_reviewer: Option<&ReviewerSpec>,
    primary_coder_agent: &str,
    pool: &[CandidateStatus],
) -> Option<ReviewerSpec> {
    if let Some(pr) = planned_reviewer {
        let (spec, substituted) = match_healthy_candidate(
            Some(&pr.agent),
            pr.model.as_deref(),
            pr.variant.as_deref(),
            pr.machine.as_deref(),
            pool,
        );
        if !substituted {
            return Some(spec);
        }
    }
    if let Some(dr) = default_reviewer {
        let (spec, substituted) = match_healthy_candidate(
            Some(&dr.agent),
            dr.model.as_deref(),
            dr.variant.as_deref(),
            dr.machine.as_deref(),
            pool,
        );
        if !substituted && !spec.agent.eq_ignore_ascii_case(primary_coder_agent) {
            return Some(spec);
        }
    }
    if let Some(diff_agent) = pool
        .iter()
        .find(|c| !c.spec.agent.eq_ignore_ascii_case(primary_coder_agent))
    {
        return Some(diff_agent.spec.clone());
    }
    default_reviewer
        .cloned()
        .or_else(|| pool.first().map(|c| c.spec.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy(agent: &str) -> CandidateStatus {
        CandidateStatus {
            spec: ReviewerSpec {
                agent: agent.into(),
                model: None,
                variant: None,
                machine: None,
            },
            health: "healthy".into(),
            reason: None,
        }
    }

    #[test]
    fn parses_parallel_dag_and_enables_worktrees_only_for_concurrent_writes() {
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
            healthy("cursor"),
            healthy("claude"),
        ];
        let reply = r#"Analyzed the task.
MAO_PLAN_JSON:
```json
{
  "complexity": "parallel_dag",
  "analysis": "Split API backend and Desktop UI changes into parallel worktrees.",
  "tasks": [
    {
      "id": "api",
      "title": "Add Gateway endpoint",
      "goal": "Implement POST /api/foo",
      "agent": "codex",
      "reason": "Rust backend",
      "depends_on": [],
      "owns_paths": ["src/"],
      "read_only": false
    },
    {
      "id": "ui",
      "title": "Add Desktop panel",
      "goal": "Wire panel to /api/foo",
      "agent": "claude",
      "reason": "Desktop UI",
      "depends_on": [],
      "owns_paths": ["desktop/"],
      "read_only": false
    }
  ],
  "verify_commands": ["cargo test --locked"],
  "reviewer": {"agent": "claude"}
}
```"#;

        let plan = parse_plan_from_reply(reply, "fallback", &candidates, None, true);
        assert_eq!(plan.complexity, "parallel_dag");
        assert_eq!(plan.tasks.len(), 2);
        // Exhausted `codex` was automatically substituted with healthy `cursor`!
        assert_eq!(plan.tasks[0].agent, "cursor");
        assert_eq!(plan.tasks[1].agent, "claude");
        // Both parallel write tasks get git worktree isolation automatically.
        assert!(plan.tasks[0].use_worktree);
        assert!(plan.tasks[1].use_worktree);
    }

    #[test]
    fn direct_l1_plan_skips_heavyweight_reviewer() {
        let candidates = vec![healthy("cursor"), healthy("claude")];
        let reply = r#"MAO_PLAN_JSON:
```json
{
  "complexity": "direct",
  "analysis": "Simple question about session routing.",
  "tasks": [
    {
      "id": "t1",
      "title": "Explain routing",
      "goal": "Explain how routes.rs dispatches requests",
      "agent": "cursor",
      "reason": "Fast read-only explanation",
      "depends_on": [],
      "owns_paths": [],
      "read_only": true
    }
  ]
}
```"#;
        let plan = parse_plan_from_reply(reply, "fallback", &candidates, None, true);
        assert_eq!(plan.complexity, "direct");
        assert!(plan.reviewer.is_none());
        assert!(!plan.tasks[0].use_worktree);
    }
}
