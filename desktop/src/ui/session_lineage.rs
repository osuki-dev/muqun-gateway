//! Sidebar identity is independent of which session a pane is viewing.
//!
//! Child snapshots belong in the metadata inventory for tools and inspectors,
//! but a detail/open/split must not promote them into top-level conversations.

use muqun_gateway::agents::{AgentPart, AgentSessionInfo, TimelineItem};

use super::UiState;
use crate::config::{ConfigEdit, SubagentContext};

fn parent(info: &AgentSessionInfo) -> Option<&str> {
    info.parent_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != "null" && *id != info.asid.0)
}

fn child_metadata(info: &AgentSessionInfo) -> bool {
    parent(info).is_some()
        || info.mode.as_deref() == Some("subagent")
        || [info.agent_role.as_deref(), info.agent_name.as_deref()]
            .into_iter()
            .flatten()
            .any(|value| !value.trim().is_empty())
}

impl UiState {
    fn lineage_computer(&self, asid: &str, ancestor: Option<&str>) -> Option<String> {
        if self.current_computer() != "all" {
            return Some(self.current_computer().to_owned());
        }
        self.computer_of_session(asid)
            .or_else(|| ancestor.and_then(|id| self.computer_of_session(id)))
    }

    fn subagent_context(&self, asid: &str) -> Option<&SubagentContext> {
        let computer = self.lineage_computer(asid, self.selected.as_deref())?;
        let agent = self.session_by_id(asid).map(|info| info.agent_id.as_str());
        if let Some(agent) = agent {
            if let Some(exact) = self.cfg.ui.subagent_contexts.iter().rev().find(|context| {
                context.computer == computer && context.asid == asid && context.agent == agent
            }) {
                return Some(exact);
            }
        }
        self.cfg
            .ui
            .subagent_contexts
            .iter()
            .rev()
            .find(|context| context.computer == computer && context.asid == asid)
    }

    pub(super) fn is_subagent_session(&self, info: &AgentSessionInfo) -> bool {
        child_metadata(info)
            || self
                .lineage_computer(&info.asid.0, None)
                .is_some_and(|computer| self.is_subagent_session_on(&computer, info))
    }

    pub(super) fn is_subagent_session_on(&self, computer: &str, info: &AgentSessionInfo) -> bool {
        child_metadata(info)
            || self
                .cfg
                .ui
                .subagent_contexts
                .iter()
                .any(|context| context.computer == computer && context.asid == info.asid.0)
    }

    /// The actual pane selection remains the child. Only its sidebar context
    /// walks observed ancestry; unknown ancestry produces no invented selection.
    pub(super) fn sidebar_root<'a>(&'a self, asid: &'a str) -> Option<&'a str> {
        let mut current = asid;
        let mut visited = std::collections::HashSet::new();
        while visited.insert(current) {
            let info = self.session_by_id(current);
            let context = self.subagent_context(current);
            let mao_parent = self.orchestration.parent_session_of(current);
            let ancestor = info
                .and_then(parent)
                .or_else(|| context.and_then(|c| c.ancestor.as_deref()))
                .or(mao_parent);
            if let Some(ancestor) = ancestor {
                current = ancestor;
            } else if context.is_some()
                || info.is_some_and(child_metadata)
                || self.orchestration.is_internal_child_session(current)
                || (info.is_none()
                    && self
                        .lineage_computer(current, None)
                        .is_some_and(|computer| {
                            self.cfg.ui.subagent_contexts.iter().any(|context| {
                                context.computer == computer && context.asid == current
                            })
                        }))
            {
                return None;
            } else {
                return Some(current);
            }
        }
        None
    }

    pub(super) fn remember_subagent_context(&mut self, child: &str, ancestor: Option<&str>) {
        self.remember_subagent_context_with_agent(child, ancestor, None);
    }

    pub(super) fn remember_subagent_context_with_agent(
        &mut self,
        child: &str,
        ancestor: Option<&str>,
        agent_hint: Option<&str>,
    ) {
        let child = child.trim();
        if ancestor.map(str::trim) == Some(child) || child.is_empty() {
            return;
        }
        let ancestor = ancestor
            .map(str::trim)
            .filter(|id| !id.is_empty() && *id != "null" && *id != child);
        let Some(computer) = self.lineage_computer(child, ancestor) else {
            return;
        };
        let old = self.subagent_context(child).cloned();
        let agent = self
            .session_by_id(child)
            .map(|info| info.agent_id.clone())
            .filter(|agent| !agent.is_empty())
            .or_else(|| {
                agent_hint
                    .map(str::trim)
                    .filter(|agent| !agent.is_empty())
                    .map(str::to_owned)
            })
            .or_else(|| {
                old.as_ref()
                    .map(|c| c.agent.clone())
                    .filter(|agent| !agent.is_empty())
            })
            .or_else(|| {
                ancestor
                    .and_then(|id| self.session_by_id(id))
                    .map(|info| info.agent_id.clone())
                    .filter(|agent| !agent.is_empty())
            })
            .unwrap_or_default();
        let ancestor = self
            .session_by_id(child)
            .and_then(parent)
            .or(ancestor)
            .map(str::to_owned)
            .or_else(|| old.as_ref().and_then(|c| c.ancestor.clone()));
        let context = SubagentContext {
            computer,
            agent,
            asid: child.to_owned(),
            ancestor,
        };
        if old.as_ref() != Some(&context) {
            self.edit(ConfigEdit::RememberSubagent(context));
        }
        self.remove_child_recency(child);
    }

    pub(super) fn remember_session_lineage(&mut self, info: &AgentSessionInfo) {
        let Some(computer) = self.lineage_computer(&info.asid.0, parent(info)) else {
            return;
        };
        self.remember_session_lineage_on(&computer, info);
    }

    pub(super) fn remember_session_lineage_on(&mut self, computer: &str, info: &AgentSessionInfo) {
        let old = self
            .cfg
            .ui
            .subagent_contexts
            .iter()
            .rev()
            .find(|context| context.computer == computer && context.asid == info.asid.0)
            .cloned();
        if child_metadata(info) || old.is_some() {
            // The metadata may be arriving before the inventory entry exists, or
            // updating a cross-agent child whose stored agent was the parent's.
            let agent = if !info.agent_id.is_empty() {
                info.agent_id.clone()
            } else {
                old.as_ref().map(|c| c.agent.clone()).unwrap_or_default()
            };
            let context = SubagentContext {
                computer: computer.to_owned(),
                agent,
                asid: info.asid.0.clone(),
                ancestor: parent(info)
                    .map(str::to_owned)
                    .or_else(|| old.as_ref().and_then(|context| context.ancestor.clone())),
            };
            if old.as_ref() != Some(&context) {
                self.edit(ConfigEdit::RememberSubagent(context));
            }
            if self.current_computer() == computer {
                self.remove_child_recency(&info.asid.0);
            } else {
                self.sidebar_opened
                    .remove(&format!("{computer}:{}", info.asid.0));
            }
        }
    }

    fn remove_child_recency(&mut self, child: &str) {
        let Some(computer) = self.lineage_computer(child, None) else {
            return;
        };
        if let Some(time) = self.sidebar_opened.remove(&format!("{computer}:{child}")) {
            if let Some(root) = self.sidebar_root(child).map(str::to_owned) {
                let opened = self
                    .sidebar_opened
                    .entry(format!("{computer}:{root}"))
                    .or_default();
                *opened = (*opened).max(time);
            }
        }
    }

    pub(super) fn remember_timeline_lineage(&mut self, ancestor: &str, items: &[TimelineItem]) {
        for item in items {
            if let AgentPart::Tool(call) = &item.part {
                if let Some(child) = &call.child_session_id {
                    self.remember_subagent_context(child, Some(ancestor));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::inbox::Update;
    use crate::ui::{search, sessions, Mode};
    use muqun_gateway::agents::ports::AgentSessionSnapshot;
    use serde_json::json;

    fn info(id: &str) -> AgentSessionInfo {
        serde_json::from_value(json!({"asid":id,"agent_id":"codex","title":id,"status":"idle","updated_ms":super::super::now_ms()})).unwrap()
    }
    fn snapshot(info: AgentSessionInfo, timeline: Vec<TimelineItem>) -> Box<AgentSessionSnapshot> {
        Box::new(AgentSessionSnapshot {
            info,
            timeline,
            permissions: vec![],
            forms: vec![],
            inbox: vec![],
            seq: 1,
        })
    }
    fn state() -> UiState {
        let (tx, _) = crate::model::commands::channel();
        let mut state = UiState::new(tx);
        state.cfg.current_computer = "local".into();
        state.apply_updates(vec![Update::AgentSessions {
            agent: "codex".into(),
            sessions: vec![info("root")],
        }]);
        state.selected = Some("root".into());
        state.mode = Mode::Session;
        state
    }

    #[test]
    fn detail_open_and_split_keep_child_context_without_promoting_recent_or_pinned_rows() {
        let mut state = state();
        let child = "019a0000-0000-7000-8000-000000000007";
        state.cfg.general.pinned.push(child.into());
        state.open_detail("root", child);
        state.apply_updates(vec![Update::Detail {
            asid: child.into(),
            result: Ok(snapshot(info(child), vec![])),
        }]);
        assert_eq!(state.selected.as_deref(), Some("root"));
        assert_eq!(sessions::listed(&state), vec!["root"]);
        state.open_session(child);
        let mut busy = info(child);
        busy.status = muqun_gateway::agents::AgentSessionStatus::Busy;
        state.apply_updates(vec![Update::Snapshot(snapshot(busy, vec![]))]);
        assert_eq!(state.selected.as_deref(), Some(child));
        assert_eq!(sessions::listed(&state), vec!["root"]);
        assert!(search::row_selected(&state, "root"));
        assert!(!state.sidebar_opened.contains_key(&format!("local:{child}")));
        state.open_session("root");
        state.open_in_tile(child);
        assert_eq!(state.selected.as_deref(), Some(child));
        assert!(state.tile_of(child).is_some());
        assert_eq!(sessions::listed(&state), vec!["root"]);
        state.apply_updates(vec![Update::AgentSessions {
            agent: "codex".into(),
            sessions: vec![info("root")],
        }]);
        assert!(
            state.session_by_id(child).is_some(),
            "root inventory refresh retains the open child metadata"
        );
    }

    #[test]
    fn restart_and_partial_hydration_reuse_scoped_lineage_but_do_not_guess_uuid_titles() {
        let mut state = state();
        state.remember_subagent_context("child", Some("root"));
        let saved = serde_json::to_value(&state.cfg).unwrap();
        let mut restarted = self::state();
        restarted.cfg = serde_json::from_value(saved).unwrap();
        restarted.apply_updates(vec![Update::AgentSessions {
            agent: "codex".into(),
            sessions: vec![
                info("root"),
                info("child"),
                info("019a0000-0000-7000-8000-000000000009"),
            ],
        }]);
        restarted.selected = Some("child".into());
        restarted.cfg.general.pinned.push("child".into());
        assert!(!sessions::listed(&restarted).contains(&"child".into()));
        assert!(
            sessions::listed(&restarted).contains(&"019a0000-0000-7000-8000-000000000009".into())
        );
        assert_eq!(restarted.selected.as_deref(), Some("child"));
        assert_eq!(restarted.sidebar_root("child"), Some("root"));
        restarted.cfg.current_computer = "mini".into();
        assert!(
            !restarted.is_subagent_session(&info("child")),
            "computer identities cannot leak"
        );
        restarted.cfg.current_computer = "local".into();
        // Cross-agent delegations match persisted SubagentContext by (computer, asid)
        // even if `context.agent` was initially recorded as the parent's agent_id.
        let mut cross_agent_child = info("child");
        cross_agent_child.agent_id = "opencode".into();
        restarted.sessions = vec![cross_agent_child.clone()];
        assert!(
            restarted.is_subagent_session(&cross_agent_child),
            "cross-agent child matches persisted lineage by (computer, asid)"
        );
        assert_eq!(restarted.sidebar_root("child"), Some("root"));
    }

    #[test]
    fn older_configs_and_explicit_role_metadata_do_not_promote_a_child_after_sparse_updates() {
        let mut state = state();
        state.cfg = serde_json::from_value(json!({})).unwrap();
        let mut child = info("uuid-looking-child");
        child.agent_role = Some("explorer".into());
        state.merge_sessions(vec![child]);
        state.merge_sessions(vec![info("uuid-looking-child")]);
        state.selected = Some("uuid-looking-child".into());
        state.cfg.general.pinned.push("uuid-looking-child".into());
        assert!(!sessions::listed(&state).contains(&"uuid-looking-child".into()));
        assert_eq!(state.sidebar_root("uuid-looking-child"), None);
        // Subagent mode/role/name hides subagents for ALL agents, not only Codex.
        let mut non_codex_sub = info("opencode-subagent");
        non_codex_sub.agent_id = "opencode".into();
        non_codex_sub.mode = Some("subagent".into());
        state.merge_sessions(vec![non_codex_sub]);
        state.selected = Some("opencode-subagent".into());
        assert!(!sessions::listed(&state).contains(&"opencode-subagent".into()));

        let mut claude_named_sub = info("claude-named-subagent");
        claude_named_sub.agent_id = "claude".into();
        claude_named_sub.agent_name = Some("researcher".into());
        state.merge_sessions(vec![claude_named_sub]);
        state.selected = Some("claude-named-subagent".into());
        assert!(!sessions::listed(&state).contains(&"claude-named-subagent".into()));
    }

    #[test]
    fn cross_agent_remember_uses_child_agent_id_when_known() {
        let mut state = state();
        let mut child = info("cross-child");
        child.agent_id = "claude".into();
        state.sessions.push(child.clone());
        state.remember_subagent_context("cross-child", Some("root"));
        let ctx = state
            .cfg
            .ui
            .subagent_contexts
            .iter()
            .find(|c| c.asid == "cross-child")
            .unwrap();
        assert_eq!(ctx.agent, "claude");
        assert_eq!(ctx.ancestor.as_deref(), Some("root"));
        assert!(state.is_subagent_session(&child));
        assert!(!sessions::listed(&state).contains(&"cross-child".into()));
    }

    #[test]
    fn all_computers_keeps_remote_child_evidence_across_background_inventory_refreshes() {
        let mut state = state();
        state.cfg.current_computer = "all".into();
        let mut child = info("remote-child");
        child.parent_id = Some("remote-root".into());
        child.agent_role = Some("explorer".into());
        let update = |sessions| Update::Computer {
            computer: "mini".into(),
            generation: 1,
            updates: vec![Update::AgentSessions {
                agent: "codex".into(),
                sessions,
            }],
        };
        state.apply_updates(vec![update(vec![info("remote-root"), child])]);
        state.apply_updates(vec![update(vec![
            info("remote-root"),
            info("remote-child"),
        ])]);
        state.selected = Some("remote-child".into());
        assert_eq!(sessions::listed(&state), vec!["remote-root"]);
        assert_eq!(state.sidebar_root("remote-child"), Some("remote-root"));
        assert!(state
            .cfg
            .ui
            .subagent_contexts
            .iter()
            .any(|context| context.computer == "mini" && context.asid == "remote-child"));
    }

    #[test]
    fn parent_tool_hydration_reclassifies_legacy_child_rows_without_deleting_their_metadata() {
        let mut state = state();
        state.merge_sessions(vec![info("child")]);
        let tool: TimelineItem = serde_json::from_value(json!({"id":"spawn","message_id":"m1","role":"assistant","seq":1,"updated_ms":1,"part":{"type":"tool","id":"spawn","name":"task","input":{},"state":"completed","status":"completed","child_session_id":"child"}})).unwrap();
        state.apply_updates(vec![Update::Snapshot(snapshot(info("root"), vec![tool]))]);
        assert_eq!(sessions::listed(&state), vec!["root"]);
        assert!(state.session_by_id("child").is_some());
        assert_eq!(state.sidebar_root("child"), Some("root"));
    }
}
