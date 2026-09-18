use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

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

/// How long to wait between asking `GET /api/agent` again while its answer is
/// still filling in. About a second and a half in total, which covers the
/// settlement measured on 2.0.1 with room to spare, and is bounded because a
/// catalog request is a user waiting on a picker.
const AGENT_SETTLE_BACKOFF: &[Duration] = &[
    Duration::from_millis(120),
    Duration::from_millis(250),
    Duration::from_millis(400),
    Duration::from_millis(750),
];

/// Every entry in `floor` that `scoped` has not got.
///
/// Naming a directory scopes the catalog *up* -- the global entries plus the
/// project's own -- so anything missing from the scoped answer that the
/// unscoped one had is an answer that has not finished arriving. Entries are
/// matched on `id`, or on `name` for the surfaces that use that instead.
fn missing_ids(scoped: &[Value], floor: &[Value]) -> Vec<String> {
    let key = |v: &Value| {
        v.get("id")
            .or_else(|| v.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let have: HashSet<String> = scoped.iter().filter_map(key).collect();
    floor
        .iter()
        .filter_map(key)
        .filter(|id| !have.contains(id))
        .collect()
}

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

    /// A catalog list, asked again until the answer has finished arriving.
    ///
    /// These lists are snapshots, and OpenCode's own spec warns a snapshot
    /// "may precede initial plugin settlement". Measured on 2.0.1, a directory
    /// nothing has opened yet answers in stages roughly four hundred
    /// milliseconds apart: agents go `[]`, then the seven built-ins, then the
    /// user's own; skills go from a handful to all of them. The gateway used
    /// to hand over whichever stage it happened to catch -- which is why a
    /// user-defined agent went missing from the picker while the built-ins
    /// were all present, and why naming a directory could return nothing.
    ///
    /// So "not empty" is not the test. The unscoped list is the floor: naming
    /// a directory scopes the catalog *up*, never down, so the answer is not
    /// finished until it holds everything the unscoped one holds. That is
    /// asked again, briefly, and then given up on -- a picker that waits
    /// forever is worse than one that is a moment stale.
    async fn settled_list<'a, F, Fut>(
        &'a self,
        what: &'static str,
        directory: Option<&'a str>,
        fetch: F,
    ) -> Vec<Value>
    where
        F: Fn(Option<&'a str>) -> Fut,
        Fut: std::future::Future<Output = Vec<Value>>,
    {
        let floor = self.once_present(&fetch, None).await;
        if directory.is_none() {
            return floor;
        }

        let mut scoped = self.once_present(&fetch, directory).await;
        for wait in AGENT_SETTLE_BACKOFF {
            if missing_ids(&scoped, &floor).is_empty() {
                return scoped;
            }
            tokio::time::sleep(*wait).await;
            scoped = fetch(directory).await;
        }

        let missing = missing_ids(&scoped, &floor);
        if !missing.is_empty() {
            // Not necessarily wrong -- a project may switch a global entry off
            // -- but it is what the old bug looked like, so it is said out loud
            // rather than passed over.
            tracing::warn!(
                surface = what,
                directory = directory.unwrap_or("<none>"),
                missing = %missing.join(", "),
                "catalog for this directory is missing entries the unscoped one has"
            );
        }
        scoped
    }

    /// Ask until there is something to return, or the waits run out. An empty
    /// list is never a real answer here: the built-ins are always there.
    async fn once_present<'a, F, Fut>(&'a self, fetch: &F, directory: Option<&'a str>) -> Vec<Value>
    where
        F: Fn(Option<&'a str>) -> Fut,
        Fut: std::future::Future<Output = Vec<Value>>,
    {
        let mut last = fetch(directory).await;
        for wait in AGENT_SETTLE_BACKOFF {
            if !last.is_empty() {
                return last;
            }
            tokio::time::sleep(*wait).await;
            last = fetch(directory).await;
        }
        last
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
                .settled_list("agent", directory, |d| self.client.agents_or_empty(d))
                .await;
            let raw_mcp = self
                .client
                .get_mcp(directory)
                .await
                .inspect_err(|e| log("mcp", e))
                .unwrap_or_default();
            let raw_skills = self
                .settled_list("skill", directory, |d| self.client.skills_or_empty(d))
                .await;
            let raw_providers = self
                .client
                .get_providers(directory)
                .await
                .inspect_err(|e| log("provider", e))
                .unwrap_or_default();
            let raw_commands = self
                .settled_list("command", directory, |d| self.client.commands_or_empty(d))
                .await;
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

    /// Naming a directory must never return fewer agents than not naming one.
    ///
    /// That is the whole bug this guards: a directory scopes the catalog *up*
    /// -- the global agents plus the project's own -- and the empty snapshot
    /// made it scope to nothing. Run against a live OpenCode 2.0.1 with
    /// `cargo test --offline -- --ignored a_directory_never_narrows`.
    #[tokio::test]
    #[ignore = "requires a running OpenCode 2.0.1 service"]
    async fn a_directory_never_narrows_the_agent_list() {
        let Some(endpoint) = OpencodeEndpoint::discover().await else {
            eprintln!("no OpenCode service registered; skipping");
            return;
        };
        let driver = OpencodeDriver::new(endpoint);

        let global = driver.get_catalog(None).await.expect("a global catalog");
        assert!(
            !global.agents.is_empty(),
            "OpenCode always has its built-in agents"
        );

        // A directory nothing has opened is the cold case that used to answer
        // with nothing at all.
        let directory = std::env::temp_dir().join(format!(
            "muqun-catalog-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let scoped = driver
            .get_catalog(directory.to_str())
            .await
            .expect("a scoped catalog");
        let _ = std::fs::remove_dir(&directory);

        assert!(
            scoped.agents.len() >= global.agents.len(),
            "a directory adds agents, it never takes them away: {} scoped vs {} global",
            scoped.agents.len(),
            global.agents.len()
        );
        for expected in &global.agents {
            assert!(
                scoped.agents.iter().any(|a| a.id == expected.id),
                "{} is missing from the scoped catalog",
                expected.id
            );
        }
    }

    fn agent(id: &str) -> Value {
        json!({ "id": id, "name": id })
    }

    /// The unscoped list is the floor, and the whole bug is that a scoped
    /// answer arrives in stages: first nothing, then the built-ins, and only
    /// then the user's own agents. Anything the floor has and the scoped
    /// answer has not is an answer that has not finished arriving.
    #[test]
    fn what_is_missing_is_measured_against_the_unscoped_list() {
        let floor = vec![agent("build"), agent("plan"), agent("osuki-coder")];

        // The stage that caused the report: the built-ins are all there, and
        // the user's own agent is not.
        let built_ins_only = vec![agent("build"), agent("plan")];
        assert_eq!(
            missing_ids(&built_ins_only, &floor),
            vec!["osuki-coder".to_string()],
            "a full-looking list can still be missing the one that matters"
        );

        // The first stage: nothing at all.
        assert_eq!(missing_ids(&[], &floor).len(), 3);

        // Settled.
        assert!(missing_ids(&floor, &floor).is_empty());

        // A directory adds its own, and that is not missing anything.
        let mut with_project = floor.clone();
        with_project.push(agent("probe-coder"));
        assert!(missing_ids(&with_project, &floor).is_empty());

        // An entry with no id cannot be matched and is simply not counted.
        assert!(missing_ids(&[json!({ "name": "nameless" })], &[]).is_empty());
    }

    /// The waits are bounded: a catalog request is a user waiting on a picker,
    /// so a list that never settles has to be given up on rather than hung on.
    #[test]
    fn the_settle_waits_are_bounded_and_ordered() {
        assert!(!AGENT_SETTLE_BACKOFF.is_empty());
        assert!(
            AGENT_SETTLE_BACKOFF.windows(2).all(|w| w[1] >= w[0]),
            "the waits only grow"
        );
        let total: Duration = AGENT_SETTLE_BACKOFF.iter().sum();
        assert!(
            total <= Duration::from_secs(3),
            "a picker must not wait longer than a person will, got {total:?}"
        );
        assert!(
            total >= Duration::from_millis(1200),
            "and it has to cover the settlement measured on 2.0.1, got {total:?}"
        );
    }

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
