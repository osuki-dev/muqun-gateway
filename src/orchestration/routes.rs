use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{concise_excerpt, parse_verdict, store, ChangedFile, PipelineStep, Run, Start};
use crate::agents::domain::{AgentPart, AgentSessionId, TimelineRole};
use crate::tasks::{
    domain::{
        now_ms, ForwardingContext, PrincipalKind, Requested, TaskBinding, TaskId, TaskKind,
        TaskOrigin, TaskPrincipal, TaskState,
    },
    service::{DelegateRequest, Delivery, TaskView},
};
use crate::{api_error, require_device, ApiResult, AppState};

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/orchestrations", get(list).post(start))
        .route("/api/orchestrations/{id}", get(read))
        .route("/api/orchestrations/{id}/approve", post(approve))
        .route("/api/orchestrations/{id}/rework", post(rework))
        .route("/api/orchestrations/{id}/cancel", post(cancel))
}

fn internal(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    tracing::warn!(%error, "orchestration request failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "orchestration_failed",
        "Could not read or persist orchestration",
    )
}

async fn list(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let runs = store::list(&state.db).await.map_err(internal)?;
    Ok(Json(json!({"runs": runs})))
}

async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_device(&state, &headers)?;
    let run = store::read(&state.db, id)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "orchestration_not_found",
                "Orchestration not found",
            )
        })?;
    view(&state, run).await
}

fn agent_asid(binding: &TaskBinding) -> Option<(String, String)> {
    match binding {
        TaskBinding::AgentSession { agent_id, asid } => Some((agent_id.clone(), asid.clone())),
        _ => None,
    }
}

async fn latest_assistant_text(state: &AppState, agent_id: &str, asid: &str) -> Option<String> {
    let manager = state.agent_runtime.manager_for_agent(agent_id).await?;
    let snapshot = manager
        .sessions()
        .get_snapshot(&AgentSessionId(asid.to_string()))
        .await
        .ok()?;
    let last_msg_id = snapshot
        .timeline
        .iter()
        .rev()
        .find(|item| {
            item.role == TimelineRole::Assistant && matches!(item.part, AgentPart::Text { .. })
        })?
        .message_id
        .clone();
    let mut text = String::new();
    for item in &snapshot.timeline {
        if item.role == TimelineRole::Assistant && item.message_id == last_msg_id {
            if let AgentPart::Text { text: chunk } = &item.part {
                if !text.is_empty() && !chunk.is_empty() {
                    text.push('\n');
                }
                text.push_str(chunk);
            }
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

fn collect_task_tree_evidence(tasks: &[TaskView], root_id: &str, run_task_ref: &str) -> String {
    let review_prefix = format!("{run_task_ref}:review:");
    let rework_prefix = format!("{run_task_ref}:rework:");
    let mut lines = Vec::new();
    for task in tasks {
        let is_internal_step = task.client_ref.as_deref().is_some_and(|cref| {
            cref.starts_with(&review_prefix) || cref.starts_with(&rework_prefix)
        });
        if is_internal_step {
            continue;
        }
        if task.task_id == root_id && !matches!(task.binding, TaskBinding::Peer { .. }) {
            continue;
        }
        let target_label = match &task.binding {
            TaskBinding::Peer {
                peer_name,
                remote_task_id,
                ..
            } => {
                if remote_task_id.is_empty() {
                    format!("peer:{peer_name}")
                } else {
                    format!("peer:{peer_name}#{remote_task_id}")
                }
            }
            TaskBinding::AgentSession { agent_id, asid } => format!("session:{agent_id}#{asid}"),
            TaskBinding::Pane {
                session_id,
                pane_id,
                agent_id,
                ..
            } => match agent_id.as_deref() {
                Some(agent) => format!("pane:{agent}@{session_id}/{pane_id}"),
                None => format!("pane:{session_id}/{pane_id}"),
            },
            TaskBinding::None => "local".to_string(),
        };
        let kind_label = match task.kind {
            TaskKind::Delegation => "delegation",
            TaskKind::Command => "command",
        };
        let exit_note = task
            .exit_code
            .map(|c| format!(" exit={c}"))
            .unwrap_or_default();
        let goal_excerpt = concise_excerpt(&task.goal, 140);
        let summary_excerpt = task
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!(" | summary: {}", concise_excerpt(s, 200)))
            .unwrap_or_default();
        lines.push(format!(
            "- {} [{}, {}, {:?}{}] target={} | {}{}",
            task.task_id,
            kind_label,
            target_label,
            task.state,
            exit_note,
            target_label,
            goal_excerpt,
            summary_excerpt
        ));
        if lines.len() >= 24 {
            break;
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "=== Muqun MCP & Remote Computer Tasks ===\n{}\n",
            lines.join("\n")
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn collect_deliverables(
    state: &AppState,
    agent_id: &str,
    asid: Option<&str>,
    cwd: &str,
    goal: &str,
    coder_summary: &str,
    base_commit: Option<&str>,
    base_commits: &std::collections::BTreeMap<String, String>,
    tasks: &[TaskView],
    root_task_id: &str,
    run_task_ref: &str,
) -> (Vec<ChangedFile>, String) {
    let mut files = Vec::new();
    let mut summary = String::new();
    let combined_goal = if coder_summary.trim().is_empty() {
        goal.to_string()
    } else {
        format!("{goal}\n{coder_summary}")
    };
    let discovered = super::discover_git_repos(cwd, &combined_goal);
    let has_local_git = !discovered.is_empty() || !base_commits.is_empty();

    if has_local_git {
        let dir = cwd.trim().to_string();
        let goal_owned = combined_goal;
        let base_map = base_commits.clone();
        let base = base_commit.map(str::to_string);
        if let Ok((local_files, local_summary)) = tokio::task::spawn_blocking(move || {
            super::collect_local_repos_diff(&dir, &goal_owned, &base_map, base.as_deref())
        })
        .await
        {
            super::merge_changed_files(cwd, &mut files, local_files);
            if !local_summary.trim().is_empty() {
                summary.push_str(local_summary.trim());
                summary.push('\n');
            }
        }
    }

    let mut timeline_evidence = String::new();
    if let Some(asid) = asid {
        if let Some(manager) = state.agent_runtime.manager_for_agent(agent_id).await {
            if let Ok(diffs) = manager
                .sessions()
                .get_vcs_diff(&AgentSessionId(asid.to_string()), "working")
                .await
            {
                let non_empty: Vec<_> = diffs
                    .iter()
                    .filter(|item| !item.path.trim().is_empty())
                    .collect();
                if !non_empty.is_empty() {
                    let prev_count = files.len();
                    let vcs_files: Vec<ChangedFile> = non_empty
                        .iter()
                        .take(32)
                        .map(|item| ChangedFile {
                            path: item.path.clone(),
                            additions: item.additions,
                            deletions: item.deletions,
                        })
                        .collect();
                    super::merge_changed_files(cwd, &mut files, vcs_files);
                    if summary.is_empty() || files.len() > prev_count {
                        if !summary.is_empty() {
                            summary.push('\n');
                        }
                        summary.push_str("=== Changed files (session diff) ===\n");
                        for item in non_empty.iter().take(32) {
                            summary.push_str(&format!(
                                "- {} (+{} -{})\n",
                                super::normalize_changed_path(cwd, &item.path),
                                item.additions,
                                item.deletions
                            ));
                        }
                        let mut patches = String::new();
                        for item in non_empty {
                            if item.patch.trim().is_empty() {
                                continue;
                            }
                            let chunk = format!("\n--- {}\n{}", item.path, item.patch);
                            if patches.len() + chunk.len() > 8000 {
                                break;
                            }
                            patches.push_str(&chunk);
                        }
                        if !patches.is_empty() {
                            summary.push_str("\nDiff excerpt:\n");
                            summary.push_str(&patches);
                            summary.push('\n');
                        }
                    }
                }
            }

            if let Ok(snapshot) = manager
                .sessions()
                .get_snapshot(&AgentSessionId(asid.to_string()))
                .await
            {
                let turn_start = snapshot
                    .timeline
                    .iter()
                    .rposition(|item| item.role == TimelineRole::User)
                    .map_or(0, |idx| idx + 1);
                let (tl_files, tl_summary) = super::extract_timeline_evidence(
                    snapshot.timeline[turn_start..].iter().map(|i| &i.part),
                );
                super::merge_changed_files(cwd, &mut files, tl_files);
                timeline_evidence = tl_summary;
            }
        }
    }

    if !timeline_evidence.is_empty()
        && (summary.trim().is_empty()
            || timeline_evidence.contains("tasks.")
            || timeline_evidence.contains("exec.")
            || timeline_evidence.contains("muqun"))
    {
        if !summary.is_empty() {
            summary.push('\n');
        }
        summary.push_str(timeline_evidence.trim());
        summary.push('\n');
    }

    let task_evidence = collect_task_tree_evidence(tasks, root_task_id, run_task_ref);
    if !task_evidence.is_empty() {
        if !summary.is_empty() {
            summary.push('\n');
        }
        summary.push_str(task_evidence.trim());
        summary.push('\n');
    }

    (files, summary)
}

async fn reconcile_pipeline(
    state: &AppState,
    run: &mut Run,
    tasks: &mut Vec<TaskView>,
    root: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    let mut dirty = false;
    let now = now_ms();
    let root_task = tasks.iter().find(|t| t.task_id == root).cloned();
    let active_coder_task = run
        .state
        .steps
        .iter()
        .rev()
        .find(|s| matches!(s.kind.as_str(), "coding" | "rework") && s.task_id.is_some())
        .and_then(|s| s.task_id.as_deref())
        .and_then(|tid| tasks.iter().find(|t| t.task_id == tid))
        .cloned()
        .or_else(|| root_task.clone());

    if let Some(ref rt) = root_task {
        if let Some((_, ref asid)) = agent_asid(&rt.binding) {
            if run.state.coder_asid.as_deref() != Some(asid.as_str()) {
                run.state.coder_asid = Some(asid.clone());
                for step in &mut run.state.steps {
                    if step.asid.is_none() && (step.kind == "coding" || step.kind == "rework") {
                        step.asid = Some(asid.clone());
                    }
                }
                dirty = true;
            }
        }
        if let Some(first_step) = run.state.steps.first_mut() {
            if first_step.task_id.is_none() {
                first_step.task_id = Some(rt.task_id.clone());
                dirty = true;
            }
        }
    }

    if let Some(ref ct) = active_coder_task {
        if ct.state == TaskState::Failed && run.state.mao_phase == "coding" {
            run.state.mao_phase = "failed".into();
            if let Some(step) = run.state.steps.last_mut() {
                if step.status == "running" {
                    step.status = "failed".into();
                }
            }
            dirty = true;
        } else if ct.state == TaskState::Cancelled && run.state.mao_phase == "coding" {
            run.state.mao_phase = "cancelled".into();
            if let Some(step) = run.state.steps.last_mut() {
                if step.status == "running" {
                    step.status = "cancelled".into();
                }
            }
            dirty = true;
        }
    }

    if let Some(rev_spec) = run.request.reviewer.clone() {
        let coder_settled = active_coder_task
            .as_ref()
            .is_some_and(|t| matches!(t.state, TaskState::Done | TaskState::IdleUnconfirmed));
        if run.state.mao_phase == "coding"
            && coder_settled
            && run.state.reviewed_round < run.state.round
        {
            if let Some(step) = run
                .state
                .steps
                .iter_mut()
                .rev()
                .find(|s| (s.kind == "coding" || s.kind == "rework") && s.status == "running")
            {
                step.status = "completed".into();
            }
            let coder_asid = run.state.coder_asid.clone();
            let coder_reply = if let Some(ref asid) = coder_asid {
                latest_assistant_text(state, &run.request.coordinator, asid).await
            } else {
                None
            };
            let coder_summary = coder_reply
                .or_else(|| active_coder_task.as_ref().and_then(|t| t.summary.clone()))
                .unwrap_or_else(|| "(No summary recorded yet.)".into());
            run.state.coder_summary = Some(concise_excerpt(&coder_summary, 600));

            let (files, diff_summary) = collect_deliverables(
                state,
                &run.request.coordinator,
                coder_asid.as_deref(),
                &run.request.cwd,
                &run.request.goal,
                &coder_summary,
                run.state.base_commit.as_deref(),
                &run.state.base_commits,
                tasks,
                root,
                &run.task_ref(),
            )
            .await;
            let review_prefix = format!("{}:review:", run.task_ref());
            let rework_prefix = format!("{}:rework:", run.task_ref());
            let has_child_or_peer_tasks = tasks.iter().any(|t| {
                let is_internal = t.client_ref.as_deref().is_some_and(|c| {
                    c.starts_with(&review_prefix) || c.starts_with(&rework_prefix)
                });
                !is_internal && (t.task_id != root || matches!(t.binding, TaskBinding::Peer { .. }))
            });
            if files.is_empty()
                && diff_summary.trim().is_empty()
                && run.state.files.is_empty()
                && !has_child_or_peer_tasks
            {
                let round = run.state.round;
                run.state.reviewed_round = round;
                run.state.mao_phase = "approved".into();
                run.state.accepted = true;
                run.state.verdict = Some("approved".into());
                run.state.review_summary =
                    Some("No code changes detected; skipped code review.".into());
                run.state.steps.push(PipelineStep {
                    kind: "verdict".into(),
                    round,
                    title: "Skipped review (no code changes)".into(),
                    detail: "No workspace diff detected after coder finished; accepted directly."
                        .into(),
                    status: "completed".into(),
                    asid: coder_asid.clone(),
                    task_id: root_task.as_ref().map(|t| t.task_id.clone()),
                    time_ms: now,
                });
                store::save_state(&state.db, run.id.clone(), run.state.clone())
                    .await
                    .map_err(internal)?;
                return Ok(());
            }
            if !files.is_empty() {
                run.state.files = files;
            }

            let round = run.state.round;
            let max_rounds = run.state.max_rounds;
            let prompt = run.review_prompt(round, max_rounds, &coder_summary, &diff_summary);
            let by = Requested {
                forwarding: ForwardingContext::Unknown,
                by: TaskPrincipal {
                    kind: PrincipalKind::Device,
                    id: run.owner.clone(),
                },
                origin: TaskOrigin::Device,
                parent: Some(TaskId(root.to_string())),
                name: None,
                server_id: None,
            };
            let client_ref = format!("{}:review:{round}", run.task_ref());
            let delegated = state
                .tasks
                .delegate_in_session(
                    state,
                    by,
                    DelegateRequest {
                        goal: prompt,
                        cwd: Some(run.request.cwd.clone()),
                        agent_id: Some(rev_spec.agent.clone()),
                        model: rev_spec.model.clone(),
                        variant: rev_spec.variant.clone(),
                        client_ref: Some(client_ref),
                        label: Some(format!("Review #{round}")),
                        delivery: Some(Delivery::Session),
                        computer_ids: run.request.computer_ids.clone(),
                        mode: None,
                        machine: run.request.machine.clone(),
                        mentions: run.request.mentions.clone(),
                        session_id: run.state.reviewer_asid.clone(),
                        attachments: vec![],
                    },
                    run.state.reviewer_asid.as_deref(),
                    Some(&format!("MAO Code Review #{round}")),
                    run.request.auto_approve.unwrap_or(true),
                )
                .await;

            run.state.reviewed_round = round;
            run.state.mao_phase = "reviewing".into();
            match delegated {
                Ok(task) => {
                    if let Some((_, asid)) = agent_asid(&task.binding) {
                        run.state.reviewer_asid = Some(asid);
                    }
                    run.state.reviewer_task_id = Some(task.task_id.clone());
                    run.state.steps.push(PipelineStep {
                        kind: "review".into(),
                        round,
                        title: format!("Code review #{round} · {}", rev_spec.agent),
                        detail: if diff_summary.trim().is_empty() {
                            "Reviewing workspace implementation…".into()
                        } else {
                            concise_excerpt(&diff_summary, 220)
                        },
                        status: "running".into(),
                        asid: run.state.reviewer_asid.clone(),
                        task_id: Some(task.task_id.clone()),
                        time_ms: now,
                    });
                    *tasks = state
                        .tasks
                        .tree(&TaskId(root.to_string()), None, 1000)
                        .await
                        .map_err(internal)?;
                }
                Err(error) => {
                    tracing::warn!(run_id = %run.id, %error, "failed to dispatch reviewer");
                    run.state.mao_phase = "failed".into();
                    run.state.review_summary = Some(format!("Reviewer dispatch failed: {error}"));
                    run.state.steps.push(PipelineStep {
                        kind: "review".into(),
                        round,
                        title: format!("Code review #{round} · {}", rev_spec.agent),
                        detail: error.to_string(),
                        status: "failed".into(),
                        asid: None,
                        task_id: None,
                        time_ms: now,
                    });
                }
            }
            dirty = true;
        } else if run.state.mao_phase == "reviewing" {
            let rev_task = run
                .state
                .reviewer_task_id
                .as_deref()
                .and_then(|tid| tasks.iter().find(|t| t.task_id == tid))
                .cloned();
            if let Some(ref rt) = rev_task {
                if let Some((_, asid)) = agent_asid(&rt.binding) {
                    if run.state.reviewer_asid.as_deref() != Some(asid.as_str()) {
                        run.state.reviewer_asid = Some(asid);
                        dirty = true;
                    }
                }
                match rt.state {
                    TaskState::Done | TaskState::IdleUnconfirmed => {
                        let reply = if let Some(ref rev_asid) = run.state.reviewer_asid {
                            latest_assistant_text(state, &rev_spec.agent, rev_asid)
                                .await
                                .or_else(|| rt.summary.clone())
                                .unwrap_or_default()
                        } else {
                            rt.summary.clone().unwrap_or_default()
                        };
                        let (verdict, excerpt) = parse_verdict(&reply);
                        run.state.review_summary = Some(excerpt.clone());
                        run.state.verdict = Some(verdict.into());
                        run.state.findings = super::parse_findings(&reply);
                        let round = run.state.round;
                        if let Some(step) = run
                            .state
                            .steps
                            .iter_mut()
                            .rev()
                            .find(|s| s.kind == "review" && s.round == round)
                        {
                            step.status = if verdict == "approved" {
                                "completed".into()
                            } else {
                                "warning".into()
                            };
                            step.detail = concise_excerpt(&excerpt, 220);
                        }

                        if verdict == "approved" {
                            run.state.mao_phase = "approved".into();
                            run.state.accepted = true;
                            run.state.steps.push(PipelineStep {
                                kind: "verdict".into(),
                                round,
                                title: format!("Review approved #{round}"),
                                detail: if excerpt.is_empty() {
                                    "MAO_VERDICT: APPROVED".into()
                                } else {
                                    excerpt
                                },
                                status: "completed".into(),
                                asid: run.state.reviewer_asid.clone(),
                                task_id: Some(rt.task_id.clone()),
                                time_ms: now,
                            });
                        } else if round < run.state.max_rounds {
                            let next_round = round + 1;
                            run.state.round = next_round;
                            run.state.mao_phase = "coding".into();
                            let issues_block = format_findings_for_rework(&run.state.findings);
                            let rework_goal = format!(
                                "[MAO Automated Rework · Round {next_round}/{}]\n\n\
                                 Reviewer ({}) requested changes for task:\n{}\n\n\
                                 Review feedback:\n{excerpt}{issues_block}\n\n\
                                 Fix every raised issue, re-run verification checks, and summarize your changes.",
                                run.state.max_rounds, rev_spec.agent, run.request.goal
                            );
                            let by = Requested {
                                forwarding: ForwardingContext::Unknown,
                                by: TaskPrincipal {
                                    kind: PrincipalKind::Device,
                                    id: run.owner.clone(),
                                },
                                origin: TaskOrigin::Device,
                                parent: Some(TaskId(root.to_string())),
                                name: None,
                                server_id: None,
                            };
                            let delegated = state
                                .tasks
                                .delegate_in_session(
                                    state,
                                    by,
                                    DelegateRequest {
                                        goal: rework_goal,
                                        cwd: Some(run.request.cwd.clone()),
                                        agent_id: Some(run.request.coordinator.clone()),
                                        model: run.request.model.clone(),
                                        variant: run.request.variant.clone(),
                                        client_ref: Some(format!(
                                            "{}:rework:{next_round}",
                                            run.task_ref()
                                        )),
                                        label: Some(format!("Rework #{next_round}")),
                                        delivery: Some(Delivery::Session),
                                        computer_ids: run.request.computer_ids.clone(),
                                        mode: run.request.mode.clone(),
                                        machine: run.request.machine.clone(),
                                        mentions: run.request.mentions.clone(),
                                        session_id: run.state.coder_asid.clone(),
                                        attachments: vec![],
                                    },
                                    run.state.coder_asid.as_deref(),
                                    Some(&format!("MAO Rework #{next_round}")),
                                    run.request.auto_approve.unwrap_or(true),
                                )
                                .await;
                            let rework_tid = delegated.as_ref().ok().map(|t| t.task_id.clone());
                            run.state.steps.push(PipelineStep {
                                kind: "rework".into(),
                                round: next_round,
                                title: format!(
                                    "Rework #{next_round} · {}",
                                    run.request.coordinator
                                ),
                                detail: concise_excerpt(&excerpt, 220),
                                status: if delegated.is_ok() {
                                    "running".into()
                                } else {
                                    "failed".into()
                                },
                                asid: run.state.coder_asid.clone(),
                                task_id: rework_tid,
                                time_ms: now,
                            });
                            if delegated.is_err() {
                                run.state.mao_phase = "changes_requested".into();
                            } else {
                                *tasks = state
                                    .tasks
                                    .tree(&TaskId(root.to_string()), None, 1000)
                                    .await
                                    .map_err(internal)?;
                            }
                        } else {
                            run.state.mao_phase = "changes_requested".into();
                            run.state.steps.push(PipelineStep {
                                kind: "verdict".into(),
                                round,
                                title: format!(
                                    "Changes requested ({round}/{})",
                                    run.state.max_rounds
                                ),
                                detail: concise_excerpt(&excerpt, 220),
                                status: "warning".into(),
                                asid: run.state.reviewer_asid.clone(),
                                task_id: Some(rt.task_id.clone()),
                                time_ms: now,
                            });
                        }
                        dirty = true;
                    }
                    TaskState::Failed => {
                        run.state.mao_phase = "failed".into();
                        run.state.review_summary = rt.summary.clone();
                        dirty = true;
                    }
                    TaskState::Cancelled => {
                        run.state.mao_phase = "cancelled".into();
                        dirty = true;
                    }
                    _ => {}
                }
            }
        }
    }

    if dirty {
        store::save_state(&state.db, run.id.clone(), run.state.clone())
            .await
            .map_err(internal)?;
    }
    Ok(())
}

/// The task tree is evidence of execution, never an automatic quality verdict
/// unless the configured Reviewer explicitly approved the changes.
async fn view(state: &AppState, run: Run) -> ApiResult<Json<Value>> {
    let mut run = store::recover(&state.db, run).await.map_err(internal)?;
    let Some(root) = run.task_id.clone() else {
        return Ok(Json(json!({
            "run": run,
            "phase": "dispatch_unconfirmed",
            "mao_phase": run.state.mao_phase,
            "round": run.state.round,
            "max_review_rounds": run.state.max_rounds,
            "coder": {
                "agent": run.request.coordinator,
                "model": run.request.model,
                "variant": run.request.variant,
                "asid": run.state.coder_asid,
            },
            "reviewer": run.request.reviewer.as_ref().map(|r| json!({
                "agent": r.agent,
                "model": r.model,
                "variant": r.variant,
                "asid": run.state.reviewer_asid,
            })),
            "deliverables": {
                "files": run.state.files,
                "total_additions": 0,
                "total_deletions": 0,
                "coder_summary": run.state.coder_summary,
                "review_summary": run.state.review_summary,
                "verdict": run.state.verdict,
                "findings": run.state.findings,
            },
            "quality_gates": run.state.quality_gates(),
            "steps": run.state.steps,
            "tasks": [],
            "accepted": false
        })));
    };
    let mut tasks = state
        .tasks
        .tree(&TaskId(root.clone()), None, 1000)
        .await
        .map_err(internal)?;
    reconcile_pipeline(state, &mut run, &mut tasks, &root).await?;

    let phase = if run.state.accepted {
        "completed"
    } else if run.state.mao_phase == "cancelled" {
        "cancelled"
    } else if matches!(run.state.mao_phase.as_str(), "changes_requested" | "failed") {
        "needs_attention"
    } else if tasks.is_empty() {
        "unavailable"
    } else if tasks.len() == 1000 {
        "incomplete_observation"
    } else if tasks.iter().any(|t| {
        matches!(
            t.state,
            TaskState::Queued | TaskState::Dispatching | TaskState::Working
        )
    }) {
        "working"
    } else if tasks.iter().any(|t| t.state == TaskState::Waiting) {
        "waiting"
    } else if tasks
        .iter()
        .any(|t| matches!(t.state, TaskState::Failed | TaskState::Cancelled))
    {
        "needs_attention"
    } else {
        "awaiting_review"
    };
    let truncated = tasks.len() == 1000;
    let total_additions: usize = run.state.files.iter().map(|f| f.additions).sum();
    let total_deletions: usize = run.state.files.iter().map(|f| f.deletions).sum();

    Ok(Json(json!({
        "run": run,
        "phase": phase,
        "mao_phase": run.state.mao_phase,
        "round": run.state.round,
        "max_review_rounds": run.state.max_rounds,
        "coder": {
            "agent": run.request.coordinator,
            "model": run.request.model,
            "variant": run.request.variant,
            "asid": run.state.coder_asid,
        },
        "reviewer": run.request.reviewer.as_ref().map(|r| json!({
            "agent": r.agent,
            "model": r.model,
            "variant": r.variant,
            "asid": run.state.reviewer_asid,
        })),
        "deliverables": {
            "files": run.state.files,
            "total_additions": total_additions,
            "total_deletions": total_deletions,
            "coder_summary": run.state.coder_summary,
            "review_summary": run.state.review_summary,
            "verdict": run.state.verdict,
            "findings": run.state.findings,
        },
        "quality_gates": run.state.quality_gates(),
        "steps": run.state.steps,
        "tasks": tasks,
        "truncated": truncated,
        "accepted": run.state.accepted,
    })))
}

async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Start>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Value>> {
    let owner = require_device(&state, &headers)?;
    let Json(request) = body.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid orchestration request",
        )
    })?;
    use crate::tasks::service::{task_directory, valid_goal, valid_name};
    let reviewer_valid = request.reviewer.as_ref().is_none_or(|rev| {
        valid_name(&rev.agent)
            && rev.model.as_deref().is_none_or(valid_name)
            && rev.variant.as_deref().is_none_or(valid_name)
    });
    let rounds_valid = request
        .max_review_rounds
        .is_none_or(|rounds| (1..=5).contains(&rounds));
    if !valid_goal(&request.goal)
        || request.goal.len() > 24 * 1024
        || !valid_name(&request.client_ref)
        || !valid_name(&request.coordinator)
        || request.model.as_deref().is_some_and(|v| !valid_name(v))
        || request.variant.as_deref().is_some_and(|v| !valid_name(v))
        || request.asid.as_deref().is_some_and(|v| !valid_name(v))
        || request.mode.as_deref().is_some_and(|v| !valid_name(v))
        || !reviewer_valid
        || !rounds_valid
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Supply a goal, client_ref and coordinator; goal maximum is 24 KiB",
        ));
    }
    // Replay is a read, even when the original workspace or agent is gone.
    if let Some(existing) = store::by_ref(&state.db, owner.clone(), request.client_ref.clone())
        .await
        .map_err(internal)?
    {
        return repeated(&state, existing, request).await;
    }
    let expanded =
        crate::agents::directories::expand_directory_param(Some(&request.cwd)).unwrap_or_default();
    let cwd = task_directory(&expanded)
        .map_err(|_| {
            api_error(
                StatusCode::BAD_REQUEST,
                "invalid_directory",
                "An existing absolute working directory is required",
            )
        })?
        .to_string_lossy()
        .into_owned();
    let manager = state
        .agent_runtime
        .manager_for_agent(&request.coordinator)
        .await;
    let requires_mcp = request.reviewer.is_none();
    if manager.is_none()
        || (requires_mcp
            && (!crate::mcp::inject::enabled(&state.config)
                || manager
                    .as_ref()
                    .is_none_or(|m| !m.agent().adds_mcp_servers())))
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            "coordinator_unavailable",
            if requires_mcp {
                "Coordinator requires a connected native agent with Muqun MCP tools"
            } else {
                "Coordinator agent is not connected"
            },
        ));
    }
    if let Some(asid) = request.asid.as_deref() {
        let info = manager
            .as_ref()
            .unwrap()
            .sessions()
            .get_session(asid)
            .await
            .map_err(|_| {
                api_error(
                    StatusCode::CONFLICT,
                    "coordinator_unavailable",
                    "The current agent session is unavailable",
                )
            })?;
        if info.status != crate::agents::AgentSessionStatus::Idle {
            return Err(api_error(
                StatusCode::CONFLICT,
                "coordinator_unavailable",
                "Wait for the current session to become idle",
            ));
        }
    }
    let (run, fresh) = store::reserve(&state.db, owner.clone(), request.clone())
        .await
        .map_err(internal)?;
    if !fresh {
        return repeated(&state, run, request).await;
    }
    let by = Requested {
        forwarding: ForwardingContext::Unknown,
        by: TaskPrincipal {
            kind: PrincipalKind::Device,
            id: owner,
        },
        origin: TaskOrigin::Device,
        parent: None,
        name: None,
        server_id: None,
    };
    let dispatch_goal = if run.request.reviewer.is_some() {
        run.coding_prompt()
    } else {
        run.prompt()
    };
    let start_auto_approve = request.auto_approve.unwrap_or(true);
    let delegated = state
        .tasks
        .delegate_in_session(
            &state,
            by,
            DelegateRequest {
                goal: dispatch_goal,
                cwd: Some(cwd),
                agent_id: Some(request.coordinator),
                model: request.model,
                variant: request.variant,
                client_ref: Some(run.task_ref()),
                label: Some("Orchestration".into()),
                delivery: Some(Delivery::Session),
                computer_ids: request.computer_ids,
                mode: request.mode,
                machine: request.machine,
                mentions: request.mentions,
                session_id: None,
                attachments: request.attachments,
            },
            request.asid.as_deref(),
            Some(&request.goal),
            start_auto_approve,
        )
        .await;
    let mut run = run;
    match &delegated {
        Ok(task) => {
            run.task_id = Some(task.task_id.clone());
            if let Some((_, asid)) = agent_asid(&task.binding) {
                run.state.coder_asid = Some(asid.clone());
                if let Some(first) = run.state.steps.first_mut() {
                    first.asid = Some(asid);
                }
            }
            if let Some(first) = run.state.steps.first_mut() {
                first.task_id = Some(task.task_id.clone());
            }
            let _ = store::save_state(&state.db, run.id.clone(), run.state.clone()).await;
        }
        Err(error) => {
            // Persisted reservation is deliberately not removed: a failed reply
            // cannot establish that no side effect happened in the agent runtime.
            tracing::warn!(run_id = %run.id, %error, "coordinator dispatch did not acknowledge success");
        }
    }
    view(&state, run).await
}

#[derive(Deserialize, Default)]
struct ReworkBody {
    #[serde(default)]
    feedback: Option<String>,
}

fn format_findings_for_rework(findings: &[super::ReviewFinding]) -> String {
    if findings.is_empty() {
        return String::new();
    }
    let items = findings
        .iter()
        .take(16)
        .map(|f| {
            format!(
                "- [{}] {} | {}",
                f.severity.to_ascii_uppercase(),
                f.location,
                f.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n\nStructured findings to fix:\n{items}")
}

async fn load_run_for_action(
    state: &AppState,
    id: String,
    caller: &str,
) -> Result<Run, (StatusCode, Json<Value>)> {
    let run = store::read(&state.db, id)
        .await
        .map_err(internal)?
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                "orchestration_not_found",
                "Orchestration not found",
            )
        })?;
    if run.owner != caller && caller != crate::platform::local_client::DEVICE_ID {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "orchestration_forbidden",
            "Only the device that created this orchestration can modify it",
        ));
    }
    Ok(run)
}

async fn approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let owner = require_device(&state, &headers)?;
    let mut run = load_run_for_action(&state, id, &owner).await?;
    run.state.accepted = true;
    run.state.mao_phase = "approved".into();
    run.state.verdict = Some("approved".into());
    run.state.steps.push(PipelineStep {
        kind: "verdict".into(),
        round: run.state.round,
        title: "Accepted manually".into(),
        detail: "Approved via control API".into(),
        status: "completed".into(),
        asid: None,
        task_id: None,
        time_ms: now_ms(),
    });
    store::save_state(&state.db, run.id.clone(), run.state.clone())
        .await
        .map_err(internal)?;
    view(&state, run).await
}

async fn rework(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<Option<ReworkBody>>>,
) -> ApiResult<Json<Value>> {
    let owner = require_device(&state, &headers)?;
    let mut run = load_run_for_action(&state, id, &owner).await?;
    let next_round = run.state.round.saturating_add(1);
    if next_round > run.state.max_rounds {
        run.state.max_rounds = next_round;
    }
    run.state.round = next_round;
    run.state.mao_phase = "coding".into();
    run.state.accepted = false;

    let feedback = body
        .and_then(|Json(b)| b)
        .and_then(|b| b.feedback)
        .or_else(|| run.state.review_summary.clone())
        .unwrap_or_else(|| "Continue improving based on review findings.".into());
    let issues_block = format_findings_for_rework(&run.state.findings);
    let rework_goal = format!(
        "[MAO Manual Rework · Round {next_round}/{}]\n\nGoal:\n{}\n\nFeedback:\n{feedback}{issues_block}",
        run.state.max_rounds, run.request.goal
    );
    let by = Requested {
        forwarding: ForwardingContext::Unknown,
        by: TaskPrincipal {
            kind: PrincipalKind::Device,
            id: owner,
        },
        origin: TaskOrigin::Device,
        parent: run.task_id.clone().map(TaskId),
        name: None,
        server_id: None,
    };
    let delegated = state
        .tasks
        .delegate_in_session(
            &state,
            by,
            DelegateRequest {
                goal: rework_goal,
                cwd: Some(run.request.cwd.clone()),
                agent_id: Some(run.request.coordinator.clone()),
                model: run.request.model.clone(),
                variant: run.request.variant.clone(),
                client_ref: Some(format!("{}:rework:{next_round}", run.task_ref())),
                label: Some(format!("Rework #{next_round}")),
                delivery: Some(Delivery::Session),
                computer_ids: run.request.computer_ids.clone(),
                mode: run.request.mode.clone(),
                machine: run.request.machine.clone(),
                mentions: run.request.mentions.clone(),
                session_id: run.state.coder_asid.clone(),
                attachments: vec![],
            },
            run.state.coder_asid.as_deref(),
            Some(&format!("MAO Rework #{next_round}")),
            run.request.auto_approve.unwrap_or(true),
        )
        .await;
    let rework_tid = delegated.as_ref().ok().map(|t| t.task_id.clone());
    run.state.steps.push(PipelineStep {
        kind: "rework".into(),
        round: next_round,
        title: format!("Rework #{next_round} · {}", run.request.coordinator),
        detail: concise_excerpt(&feedback, 220),
        status: if delegated.is_ok() {
            "running".into()
        } else {
            "failed".into()
        },
        asid: run.state.coder_asid.clone(),
        task_id: rework_tid,
        time_ms: now_ms(),
    });
    store::save_state(&state.db, run.id.clone(), run.state.clone())
        .await
        .map_err(internal)?;
    view(&state, run).await
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let owner = require_device(&state, &headers)?;
    let mut run = load_run_for_action(&state, id, &owner).await?;
    run.state.mao_phase = "cancelled".into();
    run.state.accepted = false;
    for step in &mut run.state.steps {
        if step.status == "running" {
            step.status = "cancelled".into();
        }
    }
    run.state.steps.push(PipelineStep {
        kind: "system".into(),
        round: run.state.round,
        title: "Stopped".into(),
        detail: "Cancelled via control API".into(),
        status: "cancelled".into(),
        asid: None,
        task_id: None,
        time_ms: now_ms(),
    });
    let principal = TaskPrincipal {
        kind: PrincipalKind::Device,
        id: owner,
    };
    if let Some(ref tid) = run.task_id {
        let _ = state
            .tasks
            .cancel(&state, &principal, &TaskId(tid.clone()))
            .await;
    }
    if let Some(ref rtid) = run.state.reviewer_task_id {
        let _ = state
            .tasks
            .cancel(&state, &principal, &TaskId(rtid.clone()))
            .await;
    }
    store::save_state(&state.db, run.id.clone(), run.state.clone())
        .await
        .map_err(internal)?;
    view(&state, run).await
}

async fn repeated(state: &AppState, run: Run, request: Start) -> ApiResult<Json<Value>> {
    if run.request != request {
        return Err(api_error(
            StatusCode::CONFLICT,
            "client_ref_conflict",
            "client_ref already belongs to a different request",
        ));
    }
    view(state, run).await
}
