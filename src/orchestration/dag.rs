//! Smart DAG orchestration engine (`strategy = "smart"`):
//!   1. Read-only analysis & live quota probe -> structured `OrchestrationPlan`
//!   2. Optional user plan confirmation with per-task agent/model overrides (`plan_ready`)
//!   3. Parallel DAG dispatch with isolated external git worktrees for concurrent write tasks
//!   4. Agent-driven integration & merge (the Integrator Agent merges subtask branches into
//!      the workspace, resolves any conflicts, runs verification, and cleans up)
//!   5. Independent cross-model review & selective findings rework

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use super::plan::{self, CandidateStatus, PlannedTask, TaskAssignmentOverride};
use super::routes::{
    agent_asid, check_candidate_quota, internal, latest_assistant_text, mark_candidate_exhausted,
    select_fallback_candidate, task_failure_reason,
};
use super::worktree::{self, RepoContext};
use super::{
    candidate_display, candidate_key, concise_excerpt, is_quota_or_capacity_error, parse_verdict,
    store, PipelineStep, ReviewerSpec, Run, Start,
};
use crate::tasks::{
    domain::{
        now_ms, ForwardingContext, PrincipalKind, Requested, TaskBinding, TaskId, TaskOrigin,
        TaskPrincipal, TaskState,
    },
    service::{DelegateRequest, Delivery, TaskView},
};
use crate::{api_error, ApiResult, AppState};

pub(super) fn is_smart_run(run: &Run) -> bool {
    run.state
        .strategy
        .as_deref()
        .or(run.request.strategy.as_deref())
        .is_some_and(|s| s.eq_ignore_ascii_case("smart"))
}

fn worktrees_state_dir() -> PathBuf {
    crate::platform::store::state_dir().unwrap_or_else(|_| std::env::temp_dir().join("muqun-mao"))
}

/// Probe live availability and real quotas for all configured pool entries,
/// falling back to all connected runtime agents when `agent_pool` is empty.
pub(super) async fn probe_candidates(state: &AppState, request: &Start) -> Vec<CandidateStatus> {
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
        match check_candidate_quota(state, &spec, false).await {
            Ok(()) => out.push(CandidateStatus {
                spec,
                health: "healthy".into(),
                reason: None,
            }),
            Err((reason, _)) => {
                let health = if reason.contains("not connected") {
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

fn pick_planner_candidate(
    request: &Start,
    candidates: &[CandidateStatus],
    exhausted: &[String],
) -> Option<ReviewerSpec> {
    if let Some(found) = candidates.iter().find(|c| {
        c.health == "healthy"
            && c.spec.agent.eq_ignore_ascii_case(&request.coordinator)
            && !super::routes::is_candidate_exhausted(exhausted, &c.spec)
    }) {
        return Some(found.spec.clone());
    }
    candidates
        .iter()
        .find(|c| {
            c.health == "healthy" && !super::routes::is_candidate_exhausted(exhausted, &c.spec)
        })
        .map(|c| c.spec.clone())
}

/// Start a smart orchestration run: probes live agent quotas, inspects repo/worktree
/// context, and dispatches a fast Read-Only Planner task to analyze the problem and
/// assign the best suited healthy agents/models.
pub(super) async fn start_smart(
    state: &AppState,
    owner: String,
    request: Start,
    cwd: String,
    original_coder_title: Option<String>,
) -> ApiResult<Run> {
    let candidates = probe_candidates(state, &request).await;
    let Some(planner) = pick_planner_candidate(&request, &candidates, &[]) else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "coordinator_unavailable",
            "No healthy agent with available quota is currently connected",
        ));
    };

    let (mut run, fresh) = store::reserve(&state.db, owner.clone(), request.clone())
        .await
        .map_err(internal)?;
    if !fresh {
        return Ok(run);
    }

    if original_coder_title.is_some() {
        run.state.original_coder_title = original_coder_title;
    }
    run.state.strategy = Some("smart".into());
    run.state.mao_phase = "analyzing".into();
    run.state.candidate_statuses = candidates.clone();
    for c in &candidates {
        if c.health == "exhausted" {
            mark_candidate_exhausted(&mut run.state.exhausted_candidates, &c.spec, true);
        }
    }

    let repo_ctx = worktree::inspect_repo(Path::new(&cwd));
    if let Some(ref repo) = repo_ctx {
        run.state.repo_root = Some(repo.toplevel.to_string_lossy().into_owned());
        run.state.integration_base_sha = Some(repo.base_sha.clone());
    }
    let allow_worktrees = request.use_worktrees.unwrap_or(true) && repo_ctx.is_some();

    let snapshot = plan::workspace_snapshot(&cwd);
    let prompt = plan::planner_prompt(
        &request.goal,
        &snapshot,
        &candidates,
        request.reviewer.as_ref(),
        allow_worktrees,
    );

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

    let delegated = state
        .tasks
        .delegate_in_session(
            state,
            by,
            DelegateRequest {
                goal: prompt,
                cwd: Some(cwd),
                agent_id: Some(planner.agent.clone()),
                model: planner.model.clone(),
                variant: planner.variant.clone(),
                client_ref: Some(run.task_ref()),
                label: Some("MAO Plan".into()),
                delivery: Some(Delivery::Session),
                computer_ids: request.computer_ids.clone(),
                mode: None,
                machine: planner.machine.clone().or_else(|| request.machine.clone()),
                mentions: request.mentions.clone(),
                session_id: None,
                attachments: request.attachments.clone(),
            },
            None,
            Some("MAO Problem Analysis & Routing"),
            true,
            None,
        )
        .await;

    match delegated {
        Ok(task) => {
            run.task_id = Some(task.task_id.clone());
            run.state.planner_task_id = Some(task.task_id.clone());
            let _ = store::set_task_id(&state.db, run.id.clone(), task.task_id.clone()).await;
            if let Some((_, asid)) = agent_asid(&task.binding) {
                run.state.planner_asid = Some(asid.clone());
                if run.state.coder_asid.is_none() {
                    run.state.coder_asid = Some(asid.clone());
                }
            }
            if let Some(step) = run.state.steps.first_mut() {
                step.title = format!("Analyze & plan · {}", planner.agent);
                step.asid = run.state.planner_asid.clone();
                step.task_id = Some(task.task_id.clone());
            }
            let _ = store::save_state(&state.db, run.id.clone(), run.state.clone()).await;
        }
        Err(error) => {
            tracing::warn!(run_id = %run.id, %error, "planner dispatch failed");
            run.state.mao_phase = "failed".into();
            run.state.review_summary = Some(format!("Planner dispatch failed: {error}"));
            if let Some(step) = run.state.steps.first_mut() {
                step.status = "failed".into();
                step.detail = error.to_string();
            }
            let _ = store::save_state(&state.db, run.id.clone(), run.state.clone()).await;
        }
    }

    Ok(run)
}

fn task_is_ready(task: &PlannedTask, all: &[PlannedTask]) -> bool {
    if task.status != "pending" {
        return false;
    }
    task.depends_on.iter().all(|dep_id| {
        all.iter()
            .find(|t| &t.id == dep_id)
            .is_some_and(|dep| dep.status == "completed")
    })
}

fn build_subtask_prompt(
    run: &Run,
    task: &PlannedTask,
    all_tasks: &[PlannedTask],
    handoff_note: Option<&str>,
) -> String {
    let mut sections = Vec::new();
    sections.push(format!(
        "Subtask `{}` ({}) as part of orchestrated goal:\n{}",
        task.id, task.title, run.request.goal
    ));
    sections.push(format!("Your specific assignment:\n{}", task.goal));

    if !task.owns_paths.is_empty() {
        sections.push(format!(
            "Scope / owned paths (stay within these boundaries to minimize merge conflicts):\n- {}",
            task.owns_paths.join("\n- ")
        ));
    }
    if task.read_only {
        sections.push("Constraint: READ-ONLY analysis/research task — do not modify files.".into());
    } else if task.use_worktree {
        sections.push(
            "Workspace note: You are working in an isolated Git worktree dedicated to this subtask. Make your edits, verify them, and commit your work cleanly on this branch so the integrator agent can merge it into the target workspace."
                .into(),
        );
    }

    let dep_summaries: Vec<String> = task
        .depends_on
        .iter()
        .filter_map(|dep_id| {
            let dep = all_tasks.iter().find(|t| &t.id == dep_id)?;
            let sum = dep.summary.as_deref().unwrap_or("(completed)");
            Some(format!(
                "- [{}] {} ({}): {}",
                dep.id,
                dep.title,
                dep.agent,
                concise_excerpt(sum, 400)
            ))
        })
        .collect();
    if !dep_summaries.is_empty() {
        sections.push(format!(
            "Upstream completed subtasks:\n{}",
            dep_summaries.join("\n")
        ));
    }
    if let Some(note) = handoff_note {
        sections.push(format!("Failover handoff context:\n{note}"));
    }
    sections.join("\n\n")
}

/// Dispatch all currently unblocked (`depends_on` satisfied) pending DAG subtasks in parallel.
pub(super) async fn dispatch_ready_dag_tasks(
    state: &AppState,
    run: &mut Run,
    root: &str,
) -> Result<usize, String> {
    let Some(plan_snapshot) = run.state.plan.clone() else {
        return Ok(0);
    };
    let ready_indices: Vec<usize> = plan_snapshot
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| task_is_ready(t, &plan_snapshot.tasks))
        .map(|(i, _)| i)
        .collect();
    if ready_indices.is_empty() {
        return Ok(0);
    }

    let repo_ctx: Option<RepoContext> = run
        .state
        .repo_root
        .as_deref()
        .map(Path::new)
        .and_then(worktree::inspect_repo)
        .or_else(|| worktree::inspect_repo(Path::new(&run.request.cwd)));
    let state_dir = worktrees_state_dir();
    let now = now_ms();
    let mut launched = 0usize;

    for idx in ready_indices {
        let all_snapshot = run.state.plan.as_ref().unwrap().tasks.clone();
        let task_cfg = all_snapshot[idx].clone();
        let mut effective_spec = task_cfg.spec();

        if let Err((reason, agent_wide)) =
            check_candidate_quota(state, &effective_spec, false).await
        {
            if run.auto_failover_enabled() {
                mark_candidate_exhausted(
                    &mut run.state.exhausted_candidates,
                    &effective_spec,
                    agent_wide,
                );
                let pool = effective_failover_pool(run);
                if let Some(fallback) = select_fallback_candidate(
                    state,
                    &pool,
                    &run.state.exhausted_candidates,
                    Some(&effective_spec.agent),
                    false,
                )
                .await
                {
                    let prev_label = candidate_display(&effective_spec);
                    let next_label = candidate_display(&fallback);
                    run.state.failover_count = run.state.failover_count.saturating_add(1);
                    run.state.steps.push(PipelineStep {
                        kind: "failover".into(),
                        round: run.state.round,
                        title: format!(
                            "Auto-switched `{}`: {prev_label} → {next_label}",
                            task_cfg.id
                        ),
                        detail: concise_excerpt(&reason, 220),
                        status: "warning".into(),
                        asid: None,
                        task_id: None,
                        time_ms: now,
                    });
                    effective_spec = fallback;
                    if let Some(ref mut p) = run.state.plan {
                        p.tasks[idx].agent = effective_spec.agent.clone();
                        p.tasks[idx].model = effective_spec.model.clone();
                        p.tasks[idx].variant = effective_spec.variant.clone();
                        p.tasks[idx].machine = effective_spec.machine.clone();
                    }
                }
            }
        }

        let mut target_cwd = run.request.cwd.clone();
        let mut wt_path: Option<String> = None;
        let mut wt_branch: Option<String> = None;
        if task_cfg.use_worktree && !task_cfg.read_only {
            if let Some(ref repo) = repo_ctx {
                match worktree::create_step_worktree(&state_dir, repo, &run.id, &task_cfg.id) {
                    Ok(handle) => {
                        target_cwd = handle.path.to_string_lossy().into_owned();
                        wt_path = Some(target_cwd.clone());
                        wt_branch = Some(handle.branch);
                    }
                    Err(err) => {
                        tracing::warn!(
                            run_id = %run.id,
                            step = %task_cfg.id,
                            %err,
                            "worktree creation failed; falling back to primary workspace"
                        );
                    }
                }
            }
        }

        let prompt = build_subtask_prompt(run, &task_cfg, &all_snapshot, None);
        let reuse_existing_asid = if all_snapshot.len() == 1
            && wt_path.is_none()
            && run.request.asid.is_some()
            && effective_spec
                .agent
                .eq_ignore_ascii_case(&run.request.coordinator)
        {
            run.request.asid.as_deref()
        } else {
            None
        };

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
                    goal: prompt,
                    cwd: Some(target_cwd),
                    agent_id: Some(effective_spec.agent.clone()),
                    model: effective_spec.model.clone(),
                    variant: effective_spec.variant.clone(),
                    client_ref: Some(format!("{}:step:{}", run.task_ref(), task_cfg.id)),
                    label: Some(concise_excerpt(&task_cfg.title, 48)),
                    delivery: Some(Delivery::Session),
                    computer_ids: run.request.computer_ids.clone(),
                    mode: run.request.mode.clone(),
                    machine: effective_spec
                        .machine
                        .clone()
                        .or_else(|| run.request.machine.clone()),
                    mentions: run.request.mentions.clone(),
                    session_id: None,
                    attachments: if all_snapshot.len() == 1 {
                        run.request.attachments.clone()
                    } else {
                        vec![]
                    },
                },
                reuse_existing_asid,
                Some(&task_cfg.title),
                run.request.auto_approve.unwrap_or(true),
                None,
            )
            .await;

        match delegated {
            Ok(task) => {
                let bound_asid = agent_asid(&task.binding).map(|(_, a)| a);
                if let Some(ref mut p) = run.state.plan {
                    let pt = &mut p.tasks[idx];
                    pt.status = "running".into();
                    pt.task_id = Some(task.task_id.clone());
                    pt.asid = bound_asid.clone();
                    pt.worktree_path = wt_path.clone();
                    pt.worktree_branch = wt_branch.clone();
                    pt.error = None;
                }
                if idx == 0 {
                    run.state.active_coder = Some(effective_spec.clone());
                    if run.request.asid.is_none() {
                        if let Some(ref a) = bound_asid {
                            run.state.coder_asid = Some(a.clone());
                        }
                    }
                }
                let wt_suffix = if wt_branch.is_some() {
                    " · worktree"
                } else {
                    ""
                };
                run.state.steps.push(PipelineStep {
                    kind: "coding".into(),
                    round: run.state.round,
                    title: format!("{} · {}{wt_suffix}", task_cfg.title, effective_spec.agent),
                    detail: concise_excerpt(&task_cfg.reason, 200),
                    status: "running".into(),
                    asid: bound_asid,
                    task_id: Some(task.task_id),
                    time_ms: now,
                });
                launched += 1;
            }
            Err(err) => {
                let msg = err.to_string();
                if let Some(ref mut p) = run.state.plan {
                    let pt = &mut p.tasks[idx];
                    pt.status = "failed".into();
                    pt.error = Some(msg.clone());
                }
                run.state.steps.push(PipelineStep {
                    kind: "coding".into(),
                    round: run.state.round,
                    title: format!("{} · {}", task_cfg.title, effective_spec.agent),
                    detail: concise_excerpt(&format!("Dispatch failed: {msg}"), 200),
                    status: "failed".into(),
                    asid: None,
                    task_id: None,
                    time_ms: now,
                });
            }
        }
    }

    Ok(launched)
}

fn effective_failover_pool(run: &Run) -> Vec<ReviewerSpec> {
    if !run.request.agent_pool.is_empty() {
        return run.request.agent_pool.clone();
    }
    run.state
        .candidate_statuses
        .iter()
        .filter(|c| c.health == "healthy")
        .map(|c| c.spec.clone())
        .collect()
}

/// Reconcile every phase of a Smart DAG orchestration run.
pub(super) async fn reconcile_smart_pipeline(
    state: &AppState,
    run: &mut Run,
    tasks: &mut Vec<TaskView>,
    root: &str,
) -> ApiResult<()> {
    let now = now_ms();
    let mut dirty = false;

    match run.state.mao_phase.as_str() {
        "analyzing" => {
            let planner_tid = run
                .state
                .planner_task_id
                .as_deref()
                .unwrap_or(root)
                .to_string();
            let Some(ptask) = tasks.iter().find(|t| t.task_id == planner_tid).cloned() else {
                return Ok(());
            };
            if let Some((_, asid)) = agent_asid(&ptask.binding) {
                if run.state.planner_asid.as_deref() != Some(asid.as_str()) {
                    run.state.planner_asid = Some(asid);
                    dirty = true;
                }
            }
            match ptask.state {
                TaskState::Done | TaskState::IdleUnconfirmed => {
                    let planner_agent = match &ptask.binding {
                        TaskBinding::AgentSession { agent_id, .. } => agent_id.clone(),
                        _ => run.request.coordinator.clone(),
                    };
                    let reply = if let Some(ref pasid) = run.state.planner_asid {
                        latest_assistant_text(state, &planner_agent, pasid)
                            .await
                            .or_else(|| ptask.summary.clone())
                            .unwrap_or_default()
                    } else {
                        ptask.summary.clone().unwrap_or_default()
                    };
                    let allow_worktrees =
                        run.request.use_worktrees.unwrap_or(true) && run.state.repo_root.is_some();
                    let parsed = plan::parse_plan_from_reply(
                        &reply,
                        &run.request.goal,
                        &run.state.candidate_statuses,
                        run.request.reviewer.as_ref(),
                        allow_worktrees,
                    );
                    if let Some(ref rev) = parsed.reviewer {
                        run.state.active_reviewer = Some(rev.clone());
                    }
                    if let Some(first) = parsed.tasks.first() {
                        run.state.active_coder = Some(first.spec());
                    }
                    if let Some(step) = run.state.steps.iter_mut().rev().find(|s| s.kind == "plan")
                    {
                        step.status = "completed".into();
                        step.detail = concise_excerpt(&parsed.analysis, 220);
                    }
                    let auto_execute =
                        parsed.complexity == "direct" || run.request.confirm_plan == Some(false);
                    run.state.plan = Some(parsed);
                    if auto_execute {
                        run.state.mao_phase = "executing".into();
                        let _ = dispatch_ready_dag_tasks(state, run, root).await;
                        *tasks = state
                            .tasks
                            .tree(&TaskId(root.to_string()), None, 1000)
                            .await
                            .map_err(internal)?;
                    } else {
                        run.state.mao_phase = "plan_ready".into();
                    }
                    dirty = true;
                }
                TaskState::Failed => {
                    let reason = task_failure_reason(&ptask);
                    let allow_worktrees =
                        run.request.use_worktrees.unwrap_or(true) && run.state.repo_root.is_some();
                    let fallback_plan = plan::parse_plan_from_reply(
                        "",
                        &run.request.goal,
                        &run.state.candidate_statuses,
                        run.request.reviewer.as_ref(),
                        allow_worktrees,
                    );
                    if let Some(step) = run.state.steps.iter_mut().rev().find(|s| s.kind == "plan")
                    {
                        step.status = "warning".into();
                        step.detail = concise_excerpt(
                            &format!("Planner failed ({reason}); created fallback plan"),
                            220,
                        );
                    }
                    run.state.plan = Some(fallback_plan);
                    run.state.mao_phase = if run.request.confirm_plan == Some(false) {
                        let _ = dispatch_ready_dag_tasks(state, run, root).await;
                        "executing".into()
                    } else {
                        "plan_ready".into()
                    };
                    dirty = true;
                }
                TaskState::Cancelled => {
                    run.state.mao_phase = "cancelled".into();
                    dirty = true;
                }
                _ => {}
            }
        }
        "executing" => {
            let Some(mut plan_obj) = run.state.plan.clone() else {
                return Ok(());
            };
            let base_sha = run.state.integration_base_sha.clone().unwrap_or_default();
            let tasks_snapshot = plan_obj.tasks.clone();
            let mut progressed = false;

            for task in &mut plan_obj.tasks {
                if task.status != "running" {
                    continue;
                }
                let Some(tid) = task.task_id.clone() else {
                    continue;
                };
                let Some(tv) = tasks.iter().find(|t| t.task_id == tid).cloned() else {
                    continue;
                };
                if let Some((_, asid)) = agent_asid(&tv.binding) {
                    if task.asid.as_deref() != Some(asid.as_str()) {
                        task.asid = Some(asid);
                        progressed = true;
                    }
                }
                match tv.state {
                    TaskState::Done | TaskState::IdleUnconfirmed => {
                        let summary = if let Some(ref asid) = task.asid {
                            latest_assistant_text(state, &task.agent, asid)
                                .await
                                .or_else(|| tv.summary.clone())
                                .unwrap_or_default()
                        } else {
                            tv.summary.clone().unwrap_or_default()
                        };
                        if let Some(ref wt_path) = task.worktree_path {
                            let msg = format!("mao({}): {}", task.id, task.title);
                            if let Err(err) = worktree::commit_worktree_changes(
                                Path::new(wt_path),
                                &base_sha,
                                &msg,
                            ) {
                                tracing::warn!(
                                    run_id = %run.id,
                                    step = %task.id,
                                    %err,
                                    "commit subtask worktree failed"
                                );
                            }
                        }
                        task.status = "completed".into();
                        task.summary = Some(concise_excerpt(&summary, 1200));
                        if let Some(step) = run
                            .state
                            .steps
                            .iter_mut()
                            .rev()
                            .find(|s| s.task_id.as_deref() == Some(tid.as_str()))
                        {
                            step.status = "completed".into();
                            if !summary.trim().is_empty() {
                                step.detail = concise_excerpt(&summary, 220);
                            }
                        }
                        progressed = true;
                    }
                    TaskState::Failed => {
                        let reason = task_failure_reason(&tv);
                        let spec = task.spec();
                        let quota_err = check_candidate_quota(state, &spec, false).await.err();
                        let should_fo = run.auto_failover_enabled()
                            && run.state.failover_count < 6
                            && (quota_err.is_some() || is_quota_or_capacity_error(&reason));
                        let mut failed_over = false;
                        if should_fo {
                            let agent_wide = quota_err.as_ref().is_none_or(|(_, w)| *w);
                            mark_candidate_exhausted(
                                &mut run.state.exhausted_candidates,
                                &spec,
                                agent_wide,
                            );
                            let pool = effective_failover_pool(run);
                            if let Some(next) = select_fallback_candidate(
                                state,
                                &pool,
                                &run.state.exhausted_candidates,
                                Some(&spec.agent),
                                false,
                            )
                            .await
                            {
                                let prev_label = candidate_display(&spec);
                                let next_label = candidate_display(&next);
                                run.state.failover_count =
                                    run.state.failover_count.saturating_add(1);
                                let fo_num = run.state.failover_count;
                                let partial_diff = task
                                    .worktree_path
                                    .as_deref()
                                    .map(|p| {
                                        worktree::worktree_diff_excerpt(Path::new(p), &base_sha)
                                    })
                                    .unwrap_or_default();
                                let handoff = format!(
                                    "Previous agent `{prev_label}` failed mid-task ({reason}). Continue from current workspace state without redoing completed edits.{}",
                                    if partial_diff.trim().is_empty() {
                                        String::new()
                                    } else {
                                        format!("\nPartial changes already present:\n{}", concise_excerpt(&partial_diff, 1200))
                                    }
                                );
                                let target_cwd = task
                                    .worktree_path
                                    .clone()
                                    .unwrap_or_else(|| run.request.cwd.clone());
                                let prompt = build_subtask_prompt(
                                    run,
                                    task,
                                    &tasks_snapshot,
                                    Some(&handoff),
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
                                if let Ok(new_task) = state
                                    .tasks
                                    .delegate_in_session(
                                        state,
                                        by,
                                        DelegateRequest {
                                            goal: prompt,
                                            cwd: Some(target_cwd),
                                            agent_id: Some(next.agent.clone()),
                                            model: next.model.clone(),
                                            variant: next.variant.clone(),
                                            client_ref: Some(format!(
                                                "{}:step:{}:fo:{fo_num}",
                                                run.task_ref(),
                                                task.id
                                            )),
                                            label: Some(concise_excerpt(&task.title, 48)),
                                            delivery: Some(Delivery::Session),
                                            computer_ids: run.request.computer_ids.clone(),
                                            mode: run.request.mode.clone(),
                                            machine: next
                                                .machine
                                                .clone()
                                                .or_else(|| run.request.machine.clone()),
                                            mentions: run.request.mentions.clone(),
                                            session_id: None,
                                            attachments: vec![],
                                        },
                                        None,
                                        Some(&task.title),
                                        run.request.auto_approve.unwrap_or(true),
                                        None,
                                    )
                                    .await
                                {
                                    task.agent = next.agent.clone();
                                    task.model = next.model.clone();
                                    task.variant = next.variant.clone();
                                    task.machine = next.machine.clone();
                                    task.task_id = Some(new_task.task_id.clone());
                                    task.asid = agent_asid(&new_task.binding).map(|(_, a)| a);
                                    run.state.steps.push(PipelineStep {
                                        kind: "failover".into(),
                                        round: run.state.round,
                                        title: format!(
                                            "Auto-switched `{}`: {prev_label} → {next_label}",
                                            task.id
                                        ),
                                        detail: concise_excerpt(&reason, 220),
                                        status: "warning".into(),
                                        asid: task.asid.clone(),
                                        task_id: Some(new_task.task_id),
                                        time_ms: now,
                                    });
                                    failed_over = true;
                                    progressed = true;
                                }
                            }
                        }
                        if !failed_over {
                            task.status = "failed".into();
                            task.error = Some(reason.clone());
                            if let Some(step) = run
                                .state
                                .steps
                                .iter_mut()
                                .rev()
                                .find(|s| s.task_id.as_deref() == Some(tid.as_str()))
                            {
                                step.status = "failed".into();
                                step.detail = concise_excerpt(&reason, 220);
                            }
                            progressed = true;
                        }
                    }
                    TaskState::Cancelled => {
                        task.status = "cancelled".into();
                        progressed = true;
                    }
                    _ => {}
                }
            }

            run.state.plan = Some(plan_obj);
            if progressed {
                dirty = true;
            }

            let launched = dispatch_ready_dag_tasks(state, run, root)
                .await
                .unwrap_or(0);
            if launched > 0 {
                dirty = true;
                *tasks = state
                    .tasks
                    .tree(&TaskId(root.to_string()), None, 1000)
                    .await
                    .map_err(internal)?;
            }

            if let Some(plan_now) = run.state.plan.clone() {
                let any_running = plan_now.tasks.iter().any(|t| t.status == "running");
                let any_pending_ready = plan_now
                    .tasks
                    .iter()
                    .any(|t| task_is_ready(t, &plan_now.tasks));
                if !any_running && !any_pending_ready {
                    let any_failed = plan_now.tasks.iter().any(|t| t.status == "failed");
                    let combined_summary = plan_now
                        .tasks
                        .iter()
                        .filter_map(|t| {
                            t.summary.as_deref().map(|s| {
                                format!("[{} · {}] {}", t.title, t.agent, concise_excerpt(s, 360))
                            })
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !combined_summary.is_empty() {
                        run.state.coder_summary = Some(combined_summary.clone());
                    }

                    if any_failed {
                        let errs = plan_now
                            .tasks
                            .iter()
                            .filter(|t| t.status == "failed")
                            .map(|t| {
                                format!("{}: {}", t.title, t.error.as_deref().unwrap_or("failed"))
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        run.state.mao_phase = "failed".into();
                        run.state.review_summary = Some(errs);
                        dirty = true;
                    } else if plan_now.complexity == "direct" {
                        run.state.mao_phase = "approved".into();
                        run.state.accepted = true;
                        run.state.verdict = Some("approved".into());
                        run.state.steps.push(PipelineStep {
                            kind: "verdict".into(),
                            round: run.state.round,
                            title: "Completed (L1 Direct)".into(),
                            detail: concise_excerpt(&combined_summary, 220),
                            status: "completed".into(),
                            asid: plan_now.tasks.first().and_then(|t| t.asid.clone()),
                            task_id: plan_now.tasks.first().and_then(|t| t.task_id.clone()),
                            time_ms: now,
                        });
                        dirty = true;
                    } else {
                        let wt_tasks: Vec<&PlannedTask> = plan_now
                            .tasks
                            .iter()
                            .filter(|t| t.worktree_branch.is_some())
                            .collect();
                        if !wt_tasks.is_empty() {
                            // Hand off all subtask worktree branches to an Integrator Agent
                            // inside the user's target workspace so the Agent itself merges,
                            // resolves any conflicts, runs verification, and cleans up!
                            dispatch_integrator_agent(state, run, tasks, root, &wt_tasks).await?;
                            dirty = true;
                        } else {
                            let (files, diff_summary) = super::collect_local_repos_diff_since(
                                &run.request.cwd,
                                &run.request.goal,
                                &run.state.base_commits,
                                &run.state.base_worktree_fingerprints,
                                run.state.base_commit.as_deref(),
                            );
                            run.state.files = files;
                            if run.state.files.is_empty() && diff_summary.trim().is_empty() {
                                run.state.mao_phase = "approved".into();
                                run.state.accepted = true;
                                run.state.verdict = Some("approved".into());
                                run.state.steps.push(PipelineStep {
                                    kind: "verdict".into(),
                                    round: run.state.round,
                                    title: "Completed (no workspace diff)".into(),
                                    detail: concise_excerpt(&combined_summary, 220),
                                    status: "completed".into(),
                                    asid: None,
                                    task_id: None,
                                    time_ms: now,
                                });
                            } else {
                                dispatch_smart_reviewer(
                                    state,
                                    run,
                                    tasks,
                                    root,
                                    &run.request.cwd.clone(),
                                    &diff_summary,
                                )
                                .await?;
                            }
                            dirty = true;
                        }
                    }
                }
            }
        }
        "integrating" => {
            let Some(itid) = run.state.integration_task_id.clone() else {
                return Ok(());
            };
            let Some(itv) = tasks.iter().find(|t| t.task_id == itid).cloned() else {
                return Ok(());
            };
            match itv.state {
                TaskState::Done | TaskState::IdleUnconfirmed => {
                    let int_agent = match &itv.binding {
                        TaskBinding::AgentSession { agent_id, .. } => agent_id.clone(),
                        _ => run.effective_coder().agent,
                    };
                    let int_summary = if let Some(ref iasid) = run.state.integration_asid {
                        latest_assistant_text(state, &int_agent, iasid)
                            .await
                            .or_else(|| itv.summary.clone())
                            .unwrap_or_default()
                    } else {
                        itv.summary.clone().unwrap_or_default()
                    };
                    if !int_summary.trim().is_empty() {
                        run.state.coder_summary = Some(concise_excerpt(&int_summary, 1200));
                    }
                    // Safety-net cleanup for any temporary worktrees/branches and ephemeral worktree sessions.
                    cleanup_all_run_worktrees(state, run).await;
                    if let Some(step) = run
                        .state
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|s| s.kind == "integrate")
                    {
                        step.status = "completed".into();
                        if !int_summary.trim().is_empty() {
                            step.detail = concise_excerpt(&int_summary, 220);
                        }
                    }
                    let (files, diff_summary) = super::collect_local_repos_diff_since(
                        &run.request.cwd,
                        &run.request.goal,
                        &run.state.base_commits,
                        &run.state.base_worktree_fingerprints,
                        run.state.base_commit.as_deref(),
                    );
                    run.state.files = files;
                    dispatch_smart_reviewer(
                        state,
                        run,
                        tasks,
                        root,
                        &run.request.cwd.clone(),
                        &diff_summary,
                    )
                    .await?;
                    dirty = true;
                }
                TaskState::Failed => {
                    let reason = task_failure_reason(&itv);
                    run.state.mao_phase = "failed".into();
                    run.state.review_summary =
                        Some(format!("Agent integration/merge failed: {reason}"));
                    if let Some(step) = run
                        .state
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|s| s.kind == "integrate")
                    {
                        step.status = "failed".into();
                        step.detail = concise_excerpt(&reason, 220);
                    }
                    dirty = true;
                }
                TaskState::Cancelled => {
                    run.state.mao_phase = "cancelled".into();
                    dirty = true;
                }
                _ => {}
            }
        }
        "reviewing" => {
            let Some(rev_tid) = run.state.reviewer_task_id.clone() else {
                return Ok(());
            };
            let Some(rt) = tasks.iter().find(|t| t.task_id == rev_tid).cloned() else {
                return Ok(());
            };
            if let Some((_, asid)) = agent_asid(&rt.binding) {
                if run.state.reviewer_asid.as_deref() != Some(asid.as_str()) {
                    run.state.reviewer_asid = Some(asid);
                    dirty = true;
                }
            }
            match rt.state {
                TaskState::Done | TaskState::IdleUnconfirmed => {
                    let rev_agent = run
                        .effective_reviewer()
                        .map(|r| r.agent)
                        .unwrap_or_else(|| run.request.coordinator.clone());
                    let reply = if let Some(ref rasid) = run.state.reviewer_asid {
                        latest_assistant_text(state, &rev_agent, rasid)
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
                                concise_excerpt(&excerpt, 220)
                            },
                            status: "completed".into(),
                            asid: run.state.reviewer_asid.clone(),
                            task_id: Some(rt.task_id),
                            time_ms: now,
                        });
                    } else {
                        run.state.mao_phase = "changes_requested".into();
                        run.state.steps.push(PipelineStep {
                            kind: "verdict".into(),
                            round,
                            title: format!("Changes requested #{round}"),
                            detail: concise_excerpt(&excerpt, 220),
                            status: "warning".into(),
                            asid: run.state.reviewer_asid.clone(),
                            task_id: Some(rt.task_id),
                            time_ms: now,
                        });
                    }
                    dirty = true;
                }
                TaskState::Failed => {
                    let reason = task_failure_reason(&rt);
                    let current_rev = run
                        .effective_reviewer()
                        .unwrap_or_else(|| run.effective_coder());
                    let mut failed_over = false;
                    if run.auto_failover_enabled() && run.state.failover_count < 6 {
                        mark_candidate_exhausted(
                            &mut run.state.exhausted_candidates,
                            &current_rev,
                            true,
                        );
                        let pool = effective_failover_pool(run);
                        let coder_agent = run.effective_coder().agent;
                        if let Some(next_rev) = select_fallback_candidate(
                            state,
                            &pool,
                            &run.state.exhausted_candidates,
                            Some(&coder_agent),
                            false,
                        )
                        .await
                        {
                            let prev_label = candidate_display(&current_rev);
                            let next_label = candidate_display(&next_rev);
                            if let Some(step) = run
                                .state
                                .steps
                                .iter_mut()
                                .rev()
                                .find(|s| s.kind == "review" && s.status == "running")
                            {
                                step.status = "warning".into();
                                step.detail = concise_excerpt(
                                    &format!(
                                        "Reviewer failed ({reason}); switching to {next_label}"
                                    ),
                                    220,
                                );
                            }
                            run.state.failover_count = run.state.failover_count.saturating_add(1);
                            run.state.active_reviewer = Some(next_rev.clone());
                            run.state.reviewer_asid = None;
                            if let Some(ref mut p) = run.state.plan {
                                p.reviewer = Some(next_rev.clone());
                            }
                            run.state.steps.push(PipelineStep {
                                kind: "failover".into(),
                                round: run.state.round,
                                title: format!(
                                    "Auto-switched Reviewer: {prev_label} → {next_label}"
                                ),
                                detail: concise_excerpt(&reason, 220),
                                status: "warning".into(),
                                asid: None,
                                task_id: None,
                                time_ms: now,
                            });
                            let (files, diff_summary) = super::collect_local_repos_diff_since(
                                &run.request.cwd,
                                &run.request.goal,
                                &run.state.base_commits,
                                &run.state.base_worktree_fingerprints,
                                run.state.base_commit.as_deref(),
                            );
                            if !files.is_empty() {
                                run.state.files = files;
                            }
                            dispatch_smart_reviewer(
                                state,
                                run,
                                tasks,
                                root,
                                &run.request.cwd.clone(),
                                &diff_summary,
                            )
                            .await?;
                            failed_over = true;
                        }
                    }
                    if !failed_over {
                        if let Some(step) = run
                            .state
                            .steps
                            .iter_mut()
                            .rev()
                            .find(|s| s.kind == "review" && s.status == "running")
                        {
                            step.status = "failed".into();
                            step.detail = concise_excerpt(
                                if reason.is_empty() {
                                    "Reviewer task failed"
                                } else {
                                    &reason
                                },
                                220,
                            );
                        }
                        run.state.mao_phase = "failed".into();
                        run.state.review_summary = Some(if reason.is_empty() {
                            "Reviewer task failed".into()
                        } else {
                            reason
                        });
                    }
                    dirty = true;
                }
                TaskState::Cancelled => {
                    run.state.mao_phase = "cancelled".into();
                    dirty = true;
                }
                _ => {}
            }
        }
        _ => {}
    }

    if dirty {
        store::save_state(&state.db, run.id.clone(), run.state.clone())
            .await
            .map_err(internal)?;
    }
    Ok(())
}

/// Dispatch an Integrator Agent inside the user's target workspace (`run.request.cwd`)
/// so the Agent itself merges every parallel worktree branch, resolves any conflicts,
/// runs verification checks, and removes temporary worktrees/branches.
async fn dispatch_integrator_agent(
    state: &AppState,
    run: &mut Run,
    tasks: &mut Vec<TaskView>,
    root: &str,
    wt_tasks: &[&PlannedTask],
) -> ApiResult<()> {
    let now = now_ms();
    let mut integrator = run.effective_coder();
    if check_candidate_quota(state, &integrator, false)
        .await
        .is_err()
        && run.auto_failover_enabled()
    {
        let pool = effective_failover_pool(run);
        if let Some(next) =
            select_fallback_candidate(state, &pool, &run.state.exhausted_candidates, None, false)
                .await
        {
            integrator = next;
        }
    }

    let branch_lines: Vec<String> = wt_tasks
        .iter()
        .map(|t| {
            let branch = t.worktree_branch.as_deref().unwrap_or("(unknown)");
            let path = t.worktree_path.as_deref().unwrap_or("(unknown)");
            let summary = t
                .summary
                .as_deref()
                .map(|s| concise_excerpt(s, 300))
                .unwrap_or_else(|| "(completed)".into());
            format!(
                "- Subtask `{}` ({}, agent `{}`): branch `{branch}` (worktree `{path}`)\n  Summary: {summary}",
                t.id, t.title, t.agent
            )
        })
        .collect();
    let verify_block = run
        .state
        .plan
        .as_ref()
        .filter(|p| !p.verify_commands.is_empty())
        .map(|p| {
            format!(
                "\n\nRun these verification checks after merging:\n- {}",
                p.verify_commands.join("\n- ")
            )
        })
        .unwrap_or_default();

    let prompt = format!(
        "All parallel subtasks for the goal below finished in isolated git worktrees. \
         You are the **Integrator Agent** running in the target workspace (`{}`).\n\n\
         Goal:\n{}\n\n\
         Completed worktree branches to integrate:\n{}\n\n\
         Instructions:\n\
         1. Merge or cherry-pick each subtask branch above into this workspace cleanly (preserving any pre-existing user changes).\n\
         2. Resolve any merge conflicts thoughtfully, keeping the full intent of every subtask.\n\
         3. Clean up the temporary `mao/*` worktrees and branches (`git worktree remove` + `git branch -D`) once merged.\n\
         4. Summarize the integrated changes and verification results.{verify_block}",
        run.request.cwd,
        run.request.goal,
        branch_lines.join("\n")
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

    run.state.mao_phase = "integrating".into();
    match state
        .tasks
        .delegate_in_session(
            state,
            by,
            DelegateRequest {
                goal: prompt,
                cwd: Some(run.request.cwd.clone()),
                agent_id: Some(integrator.agent.clone()),
                model: integrator.model.clone(),
                variant: integrator.variant.clone(),
                client_ref: Some(format!("{}:integrate:{}", run.task_ref(), run.state.round)),
                label: Some("Integrate & merge branches".into()),
                delivery: Some(Delivery::Session),
                computer_ids: run.request.computer_ids.clone(),
                mode: run.request.mode.clone(),
                machine: integrator
                    .machine
                    .clone()
                    .or_else(|| run.request.machine.clone()),
                mentions: vec![],
                session_id: None,
                attachments: vec![],
            },
            run.request.asid.as_deref(),
            Some("MAO Integrate Worktree Branches"),
            run.request.auto_approve.unwrap_or(true),
            None,
        )
        .await
    {
        Ok(task) => {
            run.state.integration_task_id = Some(task.task_id.clone());
            run.state.integration_asid = agent_asid(&task.binding).map(|(_, a)| a);
            run.state.steps.push(PipelineStep {
                kind: "integrate".into(),
                round: run.state.round,
                title: format!(
                    "Merge {} worktree branch(es) · {}",
                    wt_tasks.len(),
                    integrator.agent
                ),
                detail: wt_tasks
                    .iter()
                    .filter_map(|t| t.worktree_branch.as_deref())
                    .collect::<Vec<_>>()
                    .join(", "),
                status: "running".into(),
                asid: run.state.integration_asid.clone(),
                task_id: Some(task.task_id),
                time_ms: now,
            });
            *tasks = state
                .tasks
                .tree(&TaskId(root.to_string()), None, 1000)
                .await
                .map_err(internal)?;
        }
        Err(err) => {
            run.state.mao_phase = "failed".into();
            run.state.review_summary = Some(format!("Integrator dispatch failed: {err}"));
        }
    }
    Ok(())
}

async fn dispatch_smart_reviewer(
    state: &AppState,
    run: &mut Run,
    tasks: &mut Vec<TaskView>,
    root: &str,
    review_cwd: &str,
    diff_summary: &str,
) -> ApiResult<()> {
    let now = now_ms();
    let Some(mut rev_spec) = run
        .state
        .plan
        .as_ref()
        .and_then(|p| p.reviewer.clone())
        .or_else(|| run.effective_reviewer())
    else {
        run.state.mao_phase = "approved".into();
        run.state.accepted = true;
        return Ok(());
    };

    if check_candidate_quota(state, &rev_spec, false)
        .await
        .is_err()
        && run.auto_failover_enabled()
    {
        let pool = effective_failover_pool(run);
        let coder_agent = run.effective_coder().agent;
        if let Some(next) = select_fallback_candidate(
            state,
            &pool,
            &run.state.exhausted_candidates,
            Some(&coder_agent),
            false,
        )
        .await
        {
            rev_spec = next;
        }
    }
    run.state.active_reviewer = Some(rev_spec.clone());

    let round = run.state.round;
    let coder_summary = run
        .state
        .coder_summary
        .clone()
        .unwrap_or_else(|| "(See workspace diff.)".into());
    let mut prompt = run.review_prompt(round, run.state.max_rounds, &coder_summary, diff_summary);
    if let Some(ref plan) = run.state.plan {
        if !plan.verify_commands.is_empty() {
            prompt.push_str(&format!(
                "\n\nPlanned read-only verification commands to run if applicable:\n- {}",
                plan.verify_commands.join("\n- ")
            ));
        }
    }

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
    let client_ref = if run.state.failover_count > 0 {
        format!(
            "{}:review:{round}:fo:{}",
            run.task_ref(),
            run.state.failover_count
        )
    } else {
        format!("{}:review:{round}", run.task_ref())
    };
    let delegated = state
        .tasks
        .delegate_in_session(
            state,
            by,
            DelegateRequest {
                goal: prompt,
                cwd: Some(review_cwd.to_string()),
                agent_id: Some(rev_spec.agent.clone()),
                model: rev_spec.model.clone(),
                variant: rev_spec.variant.clone(),
                client_ref: Some(client_ref),
                label: Some(format!("Review #{round}")),
                delivery: Some(Delivery::Session),
                computer_ids: run.request.computer_ids.clone(),
                mode: None,
                machine: rev_spec.machine.clone(),
                mentions: run.request.mentions.clone(),
                session_id: None,
                attachments: vec![],
            },
            run.state.reviewer_asid.as_deref(),
            Some(&format!("MAO Code Review #{round}")),
            true,
            None,
        )
        .await;

    run.state.reviewed_round = round;
    match delegated {
        Ok(task) => {
            run.state.mao_phase = "reviewing".into();
            run.state.reviewer_task_id = Some(task.task_id.clone());
            if let Some((_, asid)) = agent_asid(&task.binding) {
                run.state.reviewer_asid = Some(asid);
            }
            run.state.steps.push(PipelineStep {
                kind: "review".into(),
                round,
                title: format!("Code review #{round} · {}", rev_spec.agent),
                detail: concise_excerpt(
                    if diff_summary.trim().is_empty() {
                        "Inspecting workspace changes…"
                    } else {
                        diff_summary
                    },
                    220,
                ),
                status: "running".into(),
                asid: run.state.reviewer_asid.clone(),
                task_id: Some(task.task_id),
                time_ms: now,
            });
            *tasks = state
                .tasks
                .tree(&TaskId(root.to_string()), None, 1000)
                .await
                .map_err(internal)?;
        }
        Err(err) => {
            run.state.mao_phase = "failed".into();
            run.state.review_summary = Some(format!("Reviewer dispatch failed: {err}"));
        }
    }
    Ok(())
}

/// Apply optional user overrides from the Plan Confirmation Card and start executing the DAG.
pub(super) async fn execute_plan_action(
    state: &AppState,
    run: &mut Run,
    overrides: Vec<TaskAssignmentOverride>,
    use_worktrees: Option<bool>,
) -> ApiResult<()> {
    let Some(mut plan_obj) = run.state.plan.clone() else {
        return Err(api_error(
            StatusCode::CONFLICT,
            "plan_not_ready",
            "This orchestration does not have an executable plan yet",
        ));
    };
    for ov in overrides {
        if let Some(task) = plan_obj.tasks.iter_mut().find(|t| t.id == ov.task_id) {
            if !ov.agent.trim().is_empty() {
                task.agent = ov.agent.trim().to_string();
                task.model = ov.model;
                task.variant = ov.variant;
                task.machine = ov.machine;
                task.reason = format!("User-assigned `{}`.", task.agent);
            }
        }
    }
    if let Some(wt_flag) = use_worktrees {
        let can_wt = wt_flag && run.state.repo_root.is_some();
        let multi = plan_obj.tasks.len() >= 2;
        for t in &mut plan_obj.tasks {
            if !t.read_only {
                t.use_worktree = can_wt && multi;
            }
        }
    }
    if let Some(first) = plan_obj.tasks.first() {
        run.state.active_coder = Some(first.spec());
    }
    run.state.plan = Some(plan_obj);
    run.state.mao_phase = "executing".into();

    let root = run
        .task_id
        .clone()
        .unwrap_or_else(|| format!("root_{}", run.id));
    dispatch_ready_dag_tasks(state, run, &root)
        .await
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, "dispatch_failed", &e))?;
    store::save_state(&state.db, run.id.clone(), run.state.clone())
        .await
        .map_err(internal)?;
    Ok(())
}

pub(super) async fn cleanup_all_run_worktrees(state: &AppState, run: &Run) {
    let mut paths = Vec::new();
    let mut branches = Vec::new();
    let mut ephemeral_sessions: Vec<(String, String)> = Vec::new();
    if let Some(ref plan) = run.state.plan {
        for t in &plan.tasks {
            if let Some(ref p) = t.worktree_path {
                paths.push(p.clone());
                if let Some(ref asid) = t.asid {
                    ephemeral_sessions.push((t.agent.clone(), asid.clone()));
                }
            }
            if let Some(ref b) = t.worktree_branch {
                branches.push(b.clone());
            }
        }
    }
    if let Some(ref repo_root) = run.state.repo_root {
        worktree::cleanup_worktrees(Path::new(repo_root), &paths, &branches);
    }
    // Best-effort remove ephemeral worktree-bound sessions so deleted worktrees
    // never linger as unopenable ghost sessions in Recent Sessions.
    for (agent, asid) in ephemeral_sessions {
        if let Some(mgr) = state.agent_runtime.manager_for_agent(&agent).await {
            let _ = mgr.agent().delete_session(&asid).await;
        }
    }
}
