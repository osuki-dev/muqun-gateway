use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::agent::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, ModelRef, PermissionDecision, SessionQuery,
};
use super::client::SessionListFilter;
use crate::agent::ports::engine::{AgentEngineError, AgentEnginePort, EngineFuture, FileDiffItem};
use super::client::OpencodeClient;
use super::discovery::OpencodeEndpoint;
use super::mapper;

/// The permission action OpenCode raises when a tool reaches outside the
/// session's own directory.
const EXTERNAL_DIRECTORY_ACTION: &str = "external_directory";

pub struct OpencodeDriver {
    client: Arc<OpencodeClient>,
    /// Sessions whose ruleset already carries the uploads allowance. The work
    /// is two HTTP calls, so it is done once per session per attached engine
    /// rather than on every prompt; a reconnect builds a new driver and primes
    /// again, which is the cheap side to be wrong on.
    primed: Mutex<HashSet<String>>,
}

impl OpencodeDriver {
    pub fn new(endpoint: OpencodeEndpoint) -> Self {
        Self {
            client: Arc::new(OpencodeClient::new(endpoint)),
            primed: Mutex::new(HashSet::new()),
        }
    }

    pub fn client(&self) -> &OpencodeClient {
        &self.client
    }

    /// Let the agent read the gateway's own upload directory without asking.
    ///
    /// An attachment sent from the app is written into the gateway's upload
    /// directory and handed to OpenCode as a host path. That path is outside
    /// the session's directory, so the first tool that opens it raises
    /// `permission.asked` for `external_directory` -- an approval prompt for
    /// the file the user just attached themselves, which is not a decision
    /// anyone is in a position to make usefully. The folder is ours, its
    /// contents are what this device uploaded, and nothing else is granted:
    /// the rule names that one directory and no other.
    ///
    /// The `PUT` replaces the whole session ruleset, so what is already there
    /// is read first and sent back with this rule appended. A session that
    /// already carries it is left alone.
    async fn prime_uploads_permission(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        {
            let Ok(primed) = self.primed.lock() else {
                return;
            };
            if primed.contains(session_id) {
                return;
            }
        }

        let Ok(uploads) = crate::uploads_dir() else {
            tracing::debug!("no upload directory to allow; skipping permission priming");
            return;
        };
        let resource = format!("{}/*", uploads.to_string_lossy());

        let existing = match self.client.get_session_permission_rules(session_id).await {
            Ok(rules) => rules,
            Err(err) => {
                // Not fatal: without the rule the agent still asks, which is
                // the behaviour this replaces, not something it breaks.
                tracing::debug!(session_id, %err, "could not read session permission rules");
                return;
            }
        };

        let Some(rules) = merge_uploads_rule(&existing, &resource) else {
            tracing::debug!(session_id, resource, "uploads already allowed for this session");
            self.mark_primed(session_id);
            return;
        };

        match self
            .client
            .set_session_permission_rules(session_id, &rules)
            .await
        {
            Ok(()) => {
                tracing::debug!(
                    session_id,
                    resource,
                    rules = rules.len(),
                    "allowed the gateway upload directory for this session"
                );
                self.mark_primed(session_id);
            }
            Err(err) => {
                tracing::debug!(session_id, %err, "could not set session permission rules");
            }
        }
    }

    fn mark_primed(&self, session_id: &str) {
        if let Ok(mut primed) = self.primed.lock() {
            primed.insert(session_id.to_string());
        }
    }
}

/// The ruleset to send back, or `None` when the session already allows the
/// upload directory and the `PUT` would only rewrite what is there.
///
/// Every rule the session already carries is preserved and kept in order:
/// OpenCode evaluates session rules last and lets the last match win, so
/// appending is what makes this an addition rather than a replacement of
/// whatever the owner set up.
fn merge_uploads_rule(existing: &[serde_json::Value], resource: &str) -> Option<Vec<serde_json::Value>> {
    let field = |rule: &serde_json::Value, key: &str| {
        rule.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let already = existing.iter().any(|rule| {
        field(rule, "action").as_deref() == Some(EXTERNAL_DIRECTORY_ACTION)
            && field(rule, "resource").as_deref() == Some(resource)
            && field(rule, "effect").as_deref() == Some("allow")
    });
    if already {
        return None;
    }
    let mut rules = existing.to_vec();
    rules.push(serde_json::json!({
        "action": EXTERNAL_DIRECTORY_ACTION,
        "resource": resource,
        "effect": "allow",
    }));
    Some(rules)
}

impl AgentEnginePort for OpencodeDriver {
    fn kind(&self) -> &'static str {
        "opencode"
    }

    fn probe(&self) -> EngineFuture<'_, bool> {
        Box::pin(async move {
            let req_client = reqwest::Client::new();
            Ok(self.client.endpoint.probe_healthy(&req_client).await)
        })
    }

    fn list_projects(&self) -> EngineFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let raw_projects = self.client.list_projects().await?;
            let projects = raw_projects.iter().filter_map(mapper::map_project).collect();
            Ok(projects)
        })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> EngineFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let filter = SessionListFilter {
                parent_id: query.parent_id.as_deref(),
                limit: query.limit,
                order: query.order.as_deref(),
                search: query.search.as_deref(),
                cursor: query.cursor.as_deref(),
            };
            let raw_sessions = self
                .client
                .list_sessions(query.directory.as_deref(), &filter)
                .await?;
            let sessions = raw_sessions.iter().filter_map(mapper::map_session).collect();
            Ok(sessions)
        })
    }

    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        agent: Option<&'a str>,
    ) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let raw = self.client.create_session(directory, model, agent).await?;
            let info = mapper::map_session(&raw).ok_or_else(|| {
                AgentEngineError::Protocol("Failed to parse created session".to_string())
            })?;
            // Done at birth so the first prompt with an attachment is not also
            // the first one to pay for two extra round trips.
            self.prime_uploads_permission(&info.asid.0).await;
            Ok(info)
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let raw = self.client.get_session(session_id).await?;
            mapper::map_session(&raw)
                .ok_or_else(|| AgentEngineError::SessionNotFound(session_id.to_string()))
        })
    }

    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            // An attachment is a path into the gateway's upload directory,
            // which sits outside the session's own -- so the allowance goes on
            // before the prompt that will make the agent open it, never after.
            if !attachments.is_empty() {
                self.prime_uploads_permission(session_id).await;
            }
            self.client.send_prompt(session_id, text, attachments, delivery).await?;
            Ok(())
        })
    }

    fn revert_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.revert_session(session_id, message_id).await?;
            Ok(())
        })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.interrupt(session_id).await?;
            Ok(())
        })
    }

    fn switch_model<'a>(
        &'a self,
        session_id: &'a str,
        model: &'a ModelRef,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.switch_model(session_id, model).await?;
            Ok(())
        })
    }

    fn switch_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.switch_agent(session_id, agent).await?;
            Ok(())
        })
    }

    fn find_files<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async move {
            // A failed search is an error, not an empty result set: the two
            // are indistinguishable to the caller otherwise.
            self.client.find_files(query, limit, directory).await
        })
    }

    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            let reply_str = match decision {
                PermissionDecision::Allow => "once",
                PermissionDecision::AllowAlways => "always",
                PermissionDecision::Deny => "reject",
            };
            self.client
                .reply_permission(session_id, request_id, reply_str, message)
                .await?;
            Ok(())
        })
    }

    fn reply_form<'a>(
        &'a self,
        session_id: &'a str,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.reply_form(session_id, form_id, &answers).await?;
            Ok(())
        })
    }

    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> EngineFuture<'a, AgentCatalog> {
        Box::pin(async move {
            // One failing fan-out arm must not empty the whole catalog, but it
            // is worth saying which one failed.
            let log = |what: &str, err: &AgentEngineError| {
                tracing::warn!(surface = what, %err, "catalog fan-out arm failed");
            };
            let raw_models = self
                .client
                .get_models(directory)
                .await
                .inspect_err(|e| log("model", e))
                .unwrap_or_default();
            let raw_agents = self
                .client
                .get_agents(directory)
                .await
                .inspect_err(|e| log("agent", e))
                .unwrap_or_default();
            let raw_mcp = self
                .client
                .get_mcp(directory)
                .await
                .inspect_err(|e| log("mcp", e))
                .unwrap_or_default();
            let raw_skills = self
                .client
                .get_skills(directory)
                .await
                .inspect_err(|e| log("skill", e))
                .unwrap_or_default();
            let raw_providers = self
                .client
                .get_providers(directory)
                .await
                .inspect_err(|e| log("provider", e))
                .unwrap_or_default();
            let raw_commands = self
                .client
                .get_commands(directory)
                .await
                .inspect_err(|e| log("command", e))
                .unwrap_or_default();
            let default_model = self
                .client
                .get_default_model(directory)
                .await
                .inspect_err(|e| log("model/default", e))
                .unwrap_or_default();
            let config = self
                .client
                .get_config(directory)
                .await
                .inspect_err(|e| log("config", e))
                .unwrap_or_default();

            let models = mapper::map_models(&raw_models);
            Ok(AgentCatalog {
                agents: mapper::map_agents(&raw_agents),
                mcp: mapper::map_mcp(&raw_mcp),
                skills: mapper::map_skills(&raw_skills),
                providers: mapper::map_providers(&raw_providers, &models),
                commands: mapper::map_commands(&raw_commands),
                defaults: mapper::map_catalog_defaults(default_model.as_ref(), &config),
                models,
            })
        })
    }

    fn get_vcs_diff<'a>(
        &'a self,
        session_id: &'a str,
        mode: &'a str,
    ) -> EngineFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move {
            // The diff is scoped to the session's own directory; a global diff
            // is not what the caller asked for.
            let directory = match self.client.get_session(session_id).await {
                Ok(raw) => mapper::map_session(&raw).and_then(|s| s.directory),
                Err(err) => {
                    tracing::debug!(session_id, %err, "vcs diff: session lookup failed, using server cwd");
                    None
                }
            };
            let raw_diff = self
                .client
                .get_vcs_diff(directory.as_deref(), mode, None)
                .await?;
            let mut diffs = Vec::new();
            for item in raw_diff {
                // `FileDiff.Info` names the path `file`; `path` is the v1 name.
                let path = item
                    .get("file")
                    .or_else(|| item.get("path"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let patch = item.get("patch").or_else(|| item.get("diff")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let additions = item.get("additions").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
                let deletions = item.get("deletions").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
                diffs.push(FileDiffItem {
                    path,
                    patch,
                    additions,
                    deletions,
                });
            }
            Ok(diffs)
        })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> EngineFuture<'a, Vec<crate::agent::domain::PermissionRequest>> {
        Box::pin(async move {
            let raw = self.client.get_session_permissions(session_id).await?;
            let asid = crate::agent::domain::AgentSessionId(session_id.to_string());
            Ok(raw
                .iter()
                .filter_map(|p| mapper::map_permission_request(p, &asid))
                .collect())
        })
    }

    fn get_pending_forms<'a>(
        &'a self,
        session_id: &'a str,
    ) -> EngineFuture<'a, Vec<crate::agent::domain::FormRequest>> {
        Box::pin(async move {
            let raw = self.client.get_session_forms(session_id).await?;
            let asid = crate::agent::domain::AgentSessionId(session_id.to_string());
            Ok(raw
                .iter()
                .filter_map(|f| mapper::map_form_request(f, &asid))
                .collect())
        })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> EngineFuture<'a, Vec<crate::agent::domain::TimelineItem>> {
        Box::pin(async move {
            let messages = self.client.get_messages(session_id, limit).await?;
            let asid = crate::agent::domain::AgentSessionId(session_id.to_string());
            Ok(mapper::map_messages_to_timeline(&messages, &asid))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const UPLOADS: &str = "/home/ryu/.local/share/muqun-gateway/uploads/*";

    /// The rule the gateway adds is exactly the one OpenCode asked about --
    /// the same action, the gateway's own directory and nothing wider, and
    /// `allow` rather than a blanket `ask` removal.
    #[test]
    fn the_uploads_rule_names_one_directory_and_allows_only_that() {
        let rules = merge_uploads_rule(&[], UPLOADS).expect("an empty ruleset needs the rule");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["action"], "external_directory");
        assert_eq!(rules[0]["resource"], UPLOADS);
        assert_eq!(rules[0]["effect"], "allow");
        // `Permission.Rule` declares additionalProperties:false.
        assert_eq!(rules[0].as_object().unwrap().len(), 3);
    }

    /// The `PUT` replaces the ruleset, so anything the session already had has
    /// to come back with it -- in order, because the last match wins.
    #[test]
    fn existing_session_rules_survive_the_merge_in_order() {
        let existing = vec![
            json!({ "action": "bash", "resource": "rm *", "effect": "deny" }),
            json!({ "action": "external_directory", "resource": "/etc/*", "effect": "ask" }),
        ];
        let rules = merge_uploads_rule(&existing, UPLOADS).expect("a new rule is needed");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0], existing[0]);
        assert_eq!(rules[1], existing[1]);
        assert_eq!(rules[2]["resource"], UPLOADS);
    }

    /// Priming twice must not grow the ruleset, whatever else is on it.
    #[test]
    fn a_session_that_already_allows_the_uploads_directory_is_left_alone() {
        let existing = vec![
            json!({ "action": "bash", "resource": "*", "effect": "ask" }),
            json!({ "action": "external_directory", "resource": UPLOADS, "effect": "allow" }),
        ];
        assert!(merge_uploads_rule(&existing, UPLOADS).is_none());
    }

    /// A rule for the same directory that is not an allowance is not this
    /// rule: the allowance still has to be appended, and being last it wins.
    #[test]
    fn a_denied_or_asked_uploads_rule_does_not_count_as_the_allowance() {
        for effect in ["deny", "ask"] {
            let existing = vec![json!({
                "action": "external_directory",
                "resource": UPLOADS,
                "effect": effect,
            })];
            let rules = merge_uploads_rule(&existing, UPLOADS)
                .unwrap_or_else(|| panic!("effect {effect} is not an allowance"));
            assert_eq!(rules.len(), 2);
            assert_eq!(rules[1]["effect"], "allow");
        }
    }

    /// A different directory's allowance is a different rule. The uploads
    /// directory is resolved per host, so a near-miss must not be taken for it.
    #[test]
    fn another_directorys_allowance_is_not_this_one() {
        let existing = vec![json!({
            "action": "external_directory",
            "resource": "/home/ryu/.local/share/muqun-gateway/*",
            "effect": "allow",
        })];
        assert!(merge_uploads_rule(&existing, UPLOADS).is_some());
    }
}
