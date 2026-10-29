//! Shared T3 project, model, session metadata, and provider catalog mapping.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::client::{ms_from_iso, ModelSelection};
use crate::agents::domain::{
    AgentCatalog, AgentErrorInfo, AgentProject, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, CatalogDefaults, CommandInfo, ModelInfo, ModelRef, ProviderInfo,
    ProviderModelInfo, SkillInfo,
};

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|x| !x.is_empty())
}

fn ms(v: &Value, key: &str) -> Option<u64> {
    s(v, key).and_then(ms_from_iso)
}

// ---- projects and sessions ------------------------------------------------

/// `OrchestrationProjectShell` / `OrchestrationProject` -> `AgentProject`.
pub fn map_project(raw: &Value) -> Option<AgentProject> {
    let id = s(raw, "id")?;
    let root = s(raw, "workspaceRoot")?;
    let name = s(raw, "title").map(str::to_string).unwrap_or_else(|| {
        std::path::Path::new(root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.to_string())
    });
    let vcs = raw
        .get("repositoryIdentity")
        .filter(|r| r.is_object())
        .map(|_| "git".to_string());
    Some(AgentProject {
        id: id.to_string(),
        canonical: root.to_string(),
        name,
        vcs,
        sandboxes: vec![],
        missing: !std::path::Path::new(root).is_dir(),
    })
}

/// The domain model reference for a T3 `modelSelection`.
pub fn map_model_ref(raw: &Value) -> Option<ModelRef> {
    let sel = ModelSelection::from_value(raw)?;
    let variant = sel
        .options
        .iter()
        .find(|(id, _)| id == "effort" || id == "reasoningEffort")
        .and_then(|(_, v)| v.as_str().map(str::to_string));
    Some(ModelRef {
        provider_id: sel.instance_id,
        model_id: sel.model,
        variant,
    })
}

/// A domain `ModelRef` as the T3 wire selection. `variant` is carried as the
/// `effort` option, which is the id every current driver reads.
pub fn model_selection_from_ref(model: &ModelRef) -> ModelSelection {
    ModelSelection {
        instance_id: model.provider_id.clone(),
        model: model.model_id.clone(),
        options: model
            .variant
            .as_ref()
            .map(|v| vec![("effort".to_string(), json!(v))])
            .unwrap_or_default(),
    }
}

/// Where a thread stands, from its provider session and latest turn.
///
/// `hasPendingApprovals`/`hasPendingUserInput` (shell rows only) are folded
/// in as busy: the agent is waiting on the user, which the app shows the
/// same way as running.
pub fn map_status(thread: &Value) -> (AgentSessionStatus, Option<String>, Option<AgentErrorInfo>) {
    let session = thread.get("session").filter(|v| v.is_object());
    let latest = thread.get("latestTurn").filter(|v| v.is_object());
    let turn_state = latest.and_then(|t| s(t, "state"));
    let session_status = session.and_then(|v| s(v, "status"));
    let last_error = session.and_then(|v| s(v, "lastError"));

    let outcome = match turn_state {
        Some("completed") => Some("succeeded".to_string()),
        Some("error") => Some("failed".to_string()),
        Some("interrupted") => Some("interrupted".to_string()),
        _ => None,
    };
    let error = if matches!(turn_state, Some("error")) || matches!(session_status, Some("error")) {
        Some(AgentErrorInfo {
            retry_after_ms: None,
            name: "ProviderError".into(),
            message: last_error
                .unwrap_or("the provider reported an error")
                .to_string(),
            status: None,
        })
    } else {
        None
    };

    let flag = |key: &str| thread.get(key).and_then(Value::as_bool).unwrap_or(false);
    // A turn in flight, or the agent waiting on the user, both read as busy.
    let waiting = flag("hasPendingApprovals") || flag("hasPendingUserInput");
    let status = if matches!(turn_state, Some("running")) || waiting {
        AgentSessionStatus::Busy
    } else {
        match (turn_state, session_status) {
            (Some("error"), _) => AgentSessionStatus::Failed,
            (Some("interrupted"), _) => AgentSessionStatus::Interrupted,
            (_, Some("running")) | (_, Some("starting")) => AgentSessionStatus::Busy,
            (_, Some("error")) => AgentSessionStatus::Failed,
            (_, Some("interrupted")) => AgentSessionStatus::Interrupted,
            _ => AgentSessionStatus::Idle,
        }
    };
    (status, outcome, error)
}

/// `OrchestrationThreadShell` or `OrchestrationThread` -> `AgentSessionInfo`.
/// `projects` supplies the directory when the thread has no worktree.
pub fn map_session(thread: &Value, projects: &HashMap<String, String>) -> Option<AgentSessionInfo> {
    let id = s(thread, "id")?;
    let project_id = s(thread, "projectId");
    let directory = s(thread, "worktreePath")
        .map(str::to_string)
        .or_else(|| project_id.and_then(|p| projects.get(p).cloned()));
    let (status, outcome, error) = map_status(thread);
    let deleted = s(thread, "deletedAt").is_some();
    let updated_ms = ms(thread, "updatedAt")
        .or_else(|| ms(thread, "createdAt"))
        .unwrap_or(0);
    let time_idle = thread.get("latestTurn").and_then(|t| ms(t, "completedAt"));
    // An untitled thread reads as the name it was created with; a task's
    // title ignores it (`untitled_placeholder`).
    let title = s(thread, "title")
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(super::driver::NEW_THREAD_TITLE)
        .to_string();
    Some(AgentSessionInfo {
        asid: AgentSessionId(id.to_string()),
        backend_session_id: String::new(),
        runtime: None,
        context_usage: None,
        git_info: None,
        agent_name: None,
        agent_role: None,
        agent_id: String::new(),
        title,
        mode: None,
        model: thread.get("modelSelection").and_then(map_model_ref),
        status,
        directory,
        cost: None,
        tokens: None,
        limit: None,
        parent_id: None,
        project_id: project_id.map(str::to_string),
        outcome,
        error,
        revert: None,
        fork: None,
        time_idle,
        time_viewed: None,
        deleted,
        updated_ms,
    })
}

/// A `projectId -> workspaceRoot` lookup from a shell or read-model snapshot.
pub fn project_roots(snapshot: &Value) -> HashMap<String, String> {
    snapshot
        .get("projects")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|p| Some((s(p, "id")?.to_string(), s(p, "workspaceRoot")?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Every thread of a shell snapshot, newest first, archived and deleted ones
/// left out.
fn provider_display_name(driver: &str) -> String {
    match driver {
        "claudeAgent" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "cursor" => "Cursor",
        "grok" => "Grok",
        "antigravity" => "Antigravity",
        other => other,
    }
    .to_string()
}

/// `server.getConfig()` -> `AgentCatalog`.
///
/// Providers that are not installed or enabled are still listed (with
/// `activation: disabled`) so the app can explain what the host needs; their
/// models are left off the flat model list.
pub fn map_catalog(config: &Value) -> AgentCatalog {
    let mut models = Vec::new();
    let mut providers = Vec::new();
    let mut commands = Vec::new();
    let mut skills = Vec::new();
    let mut default_model: Option<ModelRef> = None;
    let mut seen_commands = std::collections::HashSet::new();

    if let Some(list) = config.get("providers").and_then(Value::as_array) {
        for p in list {
            let Some(instance_id) = s(p, "instanceId") else {
                continue;
            };
            let driver = s(p, "driver").unwrap_or(instance_id);
            // T3's own display name wins; the fallback only covers a payload
            // that omits it.
            let name = s(p, "displayName")
                .map(str::to_string)
                .unwrap_or_else(|| provider_display_name(driver));
            let enabled = p.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let installed = p.get("installed").and_then(Value::as_bool).unwrap_or(false);
            let ready = enabled && installed && s(p, "status") != Some("disabled");
            let mut provider_models = Vec::new();
            if let Some(model_list) = p.get("models").and_then(Value::as_array) {
                for m in model_list {
                    let Some(slug) = s(m, "slug") else { continue };
                    let model_name = s(m, "name").unwrap_or(slug).to_string();
                    let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false);
                    let status = if m.get("isLegacy").and_then(Value::as_bool).unwrap_or(false) {
                        Some("deprecated".to_string())
                    } else {
                        Some("active".to_string())
                    };
                    provider_models.push(ProviderModelInfo {
                        id: slug.to_string(),
                        name: model_name.clone(),
                        enabled: ready,
                        variants: vec![],
                        limit: None,
                        status: status.clone(),
                    });
                    if ready {
                        models.push(ModelInfo {
                            id: slug.to_string(),
                            name: model_name,
                            provider_id: instance_id.to_string(),
                            family: m
                                .get("capabilities")
                                .and_then(|c| s(c, "family"))
                                .map(str::to_string),
                            limit: None,
                            variants: None,
                            cost: None,
                            enabled: true,
                            status,
                        });
                        if is_default && default_model.is_none() {
                            default_model = Some(ModelRef {
                                provider_id: instance_id.to_string(),
                                model_id: slug.to_string(),
                                variant: None,
                            });
                        }
                    }
                }
            }
            if ready {
                if let Some(list) = p.get("slashCommands").and_then(Value::as_array) {
                    for c in list {
                        if let Some(cmd_name) = s(c, "name") {
                            if seen_commands.insert(cmd_name.to_string()) {
                                commands.push(CommandInfo {
                                    name: cmd_name.to_string(),
                                    description: s(c, "description").map(str::to_string),
                                    mode: None,
                                    template: None,
                                });
                            }
                        }
                    }
                }
                if let Some(list) = p.get("skills").and_then(Value::as_array) {
                    for sk in list {
                        if let Some(skill_name) = s(sk, "name") {
                            skills.push(SkillInfo {
                                id: format!("{instance_id}:{skill_name}"),
                                name: skill_name.to_string(),
                                description: s(sk, "description").unwrap_or("").to_string(),
                                slash: false,
                                autoinvoke: false,
                            });
                        }
                    }
                }
            }
            providers.push(ProviderInfo {
                id: instance_id.to_string(),
                name,
                activation: Some(if ready { "enabled" } else { "disabled" }.to_string()),
                available: ready && !provider_models.is_empty(),
                models: provider_models,
            });
        }
    }

    // A first default: the first ready provider's first model.
    if default_model.is_none() {
        default_model = models.first().map(|m| ModelRef {
            provider_id: m.provider_id.clone(),
            model_id: m.id.clone(),
            variant: None,
        });
    }

    AgentCatalog {
        models,
        modes: vec![],
        mcp: vec![],
        skills,
        providers,
        commands,
        defaults: CatalogDefaults {
            model: default_model,
            mode: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "shell" => include_str!("fixtures/shell_snapshot.json"),
            "thread" => include_str!("fixtures/thread_detail.json"),
            "config" => include_str!("fixtures/server_config.json"),
            "approval" => include_str!("fixtures/activity_approval_requested.json"),
            "tool" => include_str!("fixtures/activity_tool_completed.json"),
            other => panic!("no fixture {other}"),
        };
        serde_json::from_str(raw).expect(name)
    }

    #[test]
    fn status_follows_the_turn_then_the_session() {
        let running = json!({"session": {"status": "running"}, "latestTurn": {"state": "running"}});
        assert_eq!(map_status(&running).0, AgentSessionStatus::Busy);
        let waiting = json!({"session": {"status": "running"}, "latestTurn": {"state": "running"}, "hasPendingApprovals": true});
        assert_eq!(map_status(&waiting).0, AgentSessionStatus::Busy);
        let interrupted =
            json!({"session": {"status": "ready"}, "latestTurn": {"state": "interrupted"}});
        let (status, outcome, _) = map_status(&interrupted);
        assert_eq!(status, AgentSessionStatus::Interrupted);
        assert_eq!(outcome.as_deref(), Some("interrupted"));
        let failed = json!({"session": {"status": "error", "lastError": "boom"}, "latestTurn": {"state": "error"}});
        let (status, _, error) = map_status(&failed);
        assert_eq!(status, AgentSessionStatus::Failed);
        assert_eq!(error.unwrap().message, "boom");
        let fresh = json!({"session": null, "latestTurn": null});
        assert_eq!(map_status(&fresh).0, AgentSessionStatus::Idle);
    }

    #[test]
    fn provider_availability_and_names_follow_the_config() {
        let catalog = map_catalog(&fixture("config"));
        let by_id = |id: &str| catalog.providers.iter().find(|p| p.id == id).unwrap();
        for id in ["codex", "claudeAgent"] {
            assert!(by_id(id).available, "{id} is ready with models");
        }
        for id in ["cursor", "grok", "opencode", "antigravity"] {
            assert!(!by_id(id).available, "{id} is disabled in T3");
        }
        assert_eq!(by_id("codex").name, "Codex");
        assert_eq!(by_id("claudeAgent").name, "Claude", "T3's own name wins");
        assert_eq!(by_id("antigravity").name, "Antigravity");
        let json = serde_json::to_value(by_id("grok")).unwrap();
        assert_eq!(json["available"], false);
        assert_eq!(json["name"], "Grok");
    }

    #[test]
    fn a_ready_provider_without_models_is_unavailable_and_names_fall_back() {
        let config = json!({"providers": [
            {"instanceId": "claudeAgent", "driver": "claudeAgent",
             "enabled": true, "installed": true, "status": "ready", "models": []},
            {"instanceId": "codex", "driver": "codex",
             "enabled": true, "installed": true, "status": "ready",
             "models": [{"slug": "m"}]},
            {"instanceId": "x", "driver": "mystery", "enabled": true,
             "installed": true, "models": [{"slug": "m"}]},
        ]});
        let catalog = map_catalog(&config);
        let p = |id: &str| catalog.providers.iter().find(|p| p.id == id).unwrap();
        assert!(!p("claudeAgent").available);
        assert_eq!(p("claudeAgent").name, "Claude Code");
        assert!(p("codex").available);
        assert_eq!(p("codex").name, "Codex");
        assert_eq!(p("x").name, "mystery");
    }

    #[test]
    fn maps_the_server_config_into_a_catalog() {
        let config = fixture("config");
        let catalog = map_catalog(&config);
        let ready: Vec<_> = catalog
            .providers
            .iter()
            .filter(|p| p.activation.as_deref() == Some("enabled"))
            .collect();
        assert!(ready.iter().any(|p| p.id == "claudeAgent"));
        assert!(ready.iter().any(|p| p.id == "codex"));
        let disabled = catalog
            .providers
            .iter()
            .find(|p| p.id == "opencode")
            .unwrap();
        assert_eq!(disabled.activation.as_deref(), Some("disabled"));
        assert!(
            catalog
                .models
                .iter()
                .all(|m| m.provider_id == "claudeAgent" || m.provider_id == "codex"),
            "only ready providers' models are offered"
        );
        assert!(catalog
            .models
            .iter()
            .any(|m| m.provider_id == "claudeAgent" && m.id.starts_with("claude-")));
        let default = catalog.defaults.model.as_ref().unwrap();
        assert_eq!(
            default.provider_id, "codex",
            "the first ready provider's default model"
        );
        assert!(catalog.defaults.mode.is_none());
        assert!(catalog.modes.is_empty(), "T3 has no modes");
        assert!(
            !catalog.commands.is_empty(),
            "slash commands come from the providers"
        );
    }

    #[test]
    fn model_refs_carry_the_effort_option_as_the_variant() {
        let sel = json!({"instanceId": "codex", "model": "gpt-5.5", "options": [{"id": "effort", "value": "high"}]});
        let r = map_model_ref(&sel).unwrap();
        assert_eq!(r.variant.as_deref(), Some("high"));
        let back = model_selection_from_ref(&r);
        assert_eq!(back.to_value(), sel);
        let plain = model_selection_from_ref(&ModelRef {
            provider_id: "a".into(),
            model_id: "b".into(),
            variant: None,
        });
        assert!(plain.options.is_empty());
    }
}
