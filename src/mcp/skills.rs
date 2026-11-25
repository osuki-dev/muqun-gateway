//! Skills over MCP (`io.modelcontextprotocol/skills`, SEP-2640) for Muqun Gateway.
//!
//! Exposes focused Muqun orchestration and cross-machine playbooks via:
//! - native SEP-2640 `skills/list` & `skills/get` (`rmcp` typed `Skill` manifests with SHA-256 digests),
//! - standard MCP `resources/list` & `resources/read` (`skill://muqun/<name>/SKILL.md`), and
//! - a compact fallback `skills.read` tool for clients that do not yet speak SEP-2640.

use rmcp::model::{
    GetSkillResult, ListResourcesResult, ListSkillsResult, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, Skill, SkillFrontmatter, SkillResource,
    SkillResources,
};
use rmcp::ErrorData;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy)]
pub(crate) struct BuiltinSkill {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub body: &'static str,
}

impl BuiltinSkill {
    pub(crate) fn uri(&self) -> String {
        format!("skill://muqun/{}/SKILL.md", self.name)
    }

    pub(crate) fn to_mcp_skill(self) -> Skill {
        let uri = self.uri();
        Skill::new(
            uri.clone(),
            SkillFrontmatter::new(self.name, self.description),
            SkillResources::Files(vec![SkillResource::from_bytes(uri, self.body.as_bytes())]),
        )
    }
}

pub(crate) const BUILTIN_SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        name: "fleet-and-exec",
        title: "Muqun Fleet Discovery & Command Execution",
        description: "Choose between fleet.describe, exec.run, and tasks.delegate across this computer and linked computers.",
        body: "\
---
name: fleet-and-exec
description: Choose between fleet.describe, exec.run, and tasks.delegate across this computer and linked computers.
---

# Muqun Fleet Discovery & Command Execution

Use this playbook before launching commands or sub-tasks on this computer or linked peers.

## 1. Inspect capabilities before routing
- Call `fleet.describe` (without `machine` for an overview, or with `machine` for details) to inspect reachable computers, OS, toolchains, installed agents, available models/modes, and live quota health.
- Prefer warm/healthy agents with available quota; avoid routing work to an agent whose quota health is degraded or exhausted.

## 2. Prefer `exec.run` for commands and short scripts
- When you only need to run a shell command, check status, run tests, or inspect files on a linked computer, call `exec.run(machine=...)` first — do **not** spin up a full coding agent with `tasks.delegate`.
- `exec.run` runs inside a visible terminal pane, records output, can be interrupted, and respects approvals.
- Fall back to your own SSH only if the target computer is not linked or `exec.run` cannot handle the case (interactive TUI session or directory outside home), and state why.

## 3. Delegate to an agent only when reasoning/editing is needed
- Use `tasks.delegate` when the target machine needs an autonomous coding agent to explore, edit code, iterate on failures, or synthesize findings.
",
    },
    BuiltinSkill {
        name: "parallel-tasks",
        title: "Muqun Parallel Fan-Out, Barrier Wakeup & Warm Reuse",
        description: "Run independent sub-tasks in parallel with barrier_group single-wakeup fan-in, batch tasks.wait, and reuse_task_id.",
        body: "\
---
name: parallel-tasks
description: Run independent sub-tasks in parallel with barrier_group single-wakeup fan-in, batch tasks.wait, and reuse_task_id.
---

# Muqun Parallel Fan-Out, Barrier Wakeup & Warm Reuse

Minimize turns, cold-start latency, and polling tokens when coordinating multiple sub-tasks.

## 1. Long parallel fan-out: use `barrier_group` and end your turn
- When delegating 2+ independent tasks that will take more than ~30 seconds, pass the **same** `barrier_group` string (e.g. `\"verify-pr-42\"`) on every `tasks.delegate` call, then end your turn.
- Muqun holds individual completion notices until every task in that `barrier_group` settles, then wakes you **once** with a consolidated summary of all results.
- If a child enters `waiting` (needs an answer), it surfaces immediately without deadlocking the batch.

## 2. Short parallel jobs (<= 45 s): use batch `tasks.wait`
- For quick jobs expected to finish within 45 seconds, call `tasks.wait(task_ids=[...], wait_for=\"all\" | \"any\")` once instead of polling tasks one by one.
- Do **not** repeatedly loop `tasks.wait` for multi-minute jobs; end your turn and let Muqun wake you when the barrier settles.

## 3. Reuse warm child sessions
- When iterating with a local child agent you already created (e.g. fixing review findings or running follow-up edits), pass `reuse_task_id` to `tasks.delegate` to continue that warm session instead of paying a cold start.
",
    },
    BuiltinSkill {
        name: "cross-machine-work",
        title: "Muqun Cross-Machine Delegation & File Sharing",
        description: "Coordinate work across linked computers safely: mentions, one-hop limits, unknown timeouts, and files.present.",
        body: "\
---
name: cross-machine-work
description: Coordinate work across linked computers safely: mentions, one-hop limits, unknown timeouts, and files.present.
---

# Muqun Cross-Machine Delegation & File Sharing

Follow these rules whenever a task spans multiple linked computers.

## 1. Check mentions and target machine
- Call `tasks.read()` (without `task_id`) to see if your current session is working on a Muqun task and which linked computers the user named (`mentions`).
- Pass `machine` (peer name or ID from `fleet.describe`) on `exec.run`, `tasks.delegate`, or `files.present`.

## 2. Handle uncertain network responses safely
- If `tasks.delegate(machine=...)` returns a `note` saying the peer did not answer in time, **do not delegate again**: wait on or read the returned `task_id` while the mirror reconciles.
- Cross-machine delegation is one hop from an incoming peer task (`delegation_loop`). If you cannot forward further, finish with `tasks.complete(outcome=\"blocked\", ...)` explaining what needs the other computer.

## 3. Share outputs without dumping huge payloads
- Use `files.present(path, machine=...)` to show images, logs, builds, or documents to the user in Muqun Desktop/App rather than pasting raw file dumps into chat.
",
    },
    BuiltinSkill {
        name: "task-completion",
        title: "Muqun Task Completion & Verification Protocol",
        description: "Close Muqun tasks cleanly with tasks.complete (done or blocked) and avoid redundant protocol text in delegated goals.",
        body: "\
---
name: task-completion
description: Close Muqun tasks cleanly with tasks.complete (done or blocked) and avoid redundant protocol text in delegated goals.
---

# Muqun Task Completion & Verification Protocol

## 1. Confirm whether this session owns a task
- If Muqun started your session for a task (or `tasks.read()` reports an active task), the requester sees the task as unfinished until you explicitly report back.
- If `tasks.complete` returns `task_not_found`, ignore the task protocol and do not print completion markers.

## 2. Report `done` or `blocked`
- After answering and verifying your changes, call `tasks.complete(summary=\"...\", outcome=\"done\")` with a concise one-line summary.
- When you cannot proceed without user input, credentials, or missing files, call `tasks.complete(summary=\"...\", outcome=\"blocked\")` with the exact blocker.
- Fallback only if the tool cannot be called: end your final reply with `MUQUN_TASK_DONE: <summary>` or `MUQUN_TASK_BLOCKED: <reason>`.

## 3. Keep delegated goals clean
- Do **not** repeat Muqun's completion protocol inside `tasks.delegate(goal=...)`; the child agent receives it automatically from Gateway.
",
    },
];

pub(crate) fn find_skill(key: &str) -> Option<&'static BuiltinSkill> {
    let trimmed = key.trim();
    BUILTIN_SKILLS
        .iter()
        .find(|skill| skill.name == trimmed || skill.uri() == trimmed)
}

pub(crate) fn list_resources() -> ListResourcesResult {
    let resources = BUILTIN_SKILLS
        .iter()
        .map(|skill| {
            Resource::new(skill.uri(), skill.name)
                .with_title(skill.title)
                .with_description(skill.description)
                .with_mime_type("text/markdown")
                .with_size(skill.body.len() as u64)
        })
        .collect();
    ListResourcesResult::with_all_items(resources)
}

pub(crate) fn read_resource(uri: &str) -> Result<ReadResourceResponse, ErrorData> {
    let Some(skill) = BUILTIN_SKILLS.iter().find(|s| s.uri() == uri) else {
        return Err(ErrorData::resource_not_found(
            format!("unknown skill resource: {uri}"),
            None,
        ));
    };
    Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
        vec![ResourceContents::TextResourceContents {
            uri: skill.uri(),
            mime_type: Some("text/markdown".to_owned()),
            text: skill.body.to_owned(),
            meta: None,
        }],
    )))
}

/// Official SEP-2640 `skills/list` response.
pub(crate) fn list_skills() -> ListSkillsResult {
    let skills = BUILTIN_SKILLS
        .iter()
        .copied()
        .map(BuiltinSkill::to_mcp_skill)
        .collect();
    ListSkillsResult::with_all_items(skills)
}

/// Official SEP-2640 `skills/get` response.
pub(crate) fn get_skill(uri: &str) -> Result<GetSkillResult, ErrorData> {
    let Some(skill) = BUILTIN_SKILLS.iter().find(|s| s.uri() == uri) else {
        return Err(ErrorData::resource_not_found(
            format!("unknown skill: {uri}"),
            None,
        ));
    };
    Ok(GetSkillResult::new(skill.to_mcp_skill()))
}

/// Compact catalog returned when `skills.read` is called without a name.
pub(crate) fn skills_catalog_json() -> Value {
    let items: Vec<Value> = BUILTIN_SKILLS
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "uri": s.uri(),
                "description": s.description,
            })
        })
        .collect();
    json!({ "skills": items })
}
