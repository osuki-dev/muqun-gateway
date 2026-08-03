//! Serialization at the legacy HTTP boundary.
//!
//! The mobile app predates the backend abstraction and consumes the Herdr
//! response envelope. Keeping that compatibility here prevents either backend
//! from shaping itself around an HTTP DTO.

use serde_json::{json, Value};

use super::{Agent, AgentStatus, Pane, PaneOutput, Tab, Workspace};

pub fn workspace_list(workspaces: Vec<Workspace>) -> Value {
    json!({
        "result": {
            "type": "workspace_list",
            "workspaces": workspaces.into_iter().map(workspace).collect::<Vec<_>>()
        }
    })
}

pub fn tab_list(tabs: Vec<Tab>) -> Value {
    json!({
        "result": {
            "type": "tab_list",
            "tabs": tabs.into_iter().map(tab).collect::<Vec<_>>()
        }
    })
}

pub fn pane_list(panes: Vec<Pane>) -> Value {
    json!({
        "result": {
            "type": "pane_list",
            "panes": panes.into_iter().map(pane).collect::<Vec<_>>()
        }
    })
}

pub fn pane_get(value: Pane) -> Value {
    json!({ "result": { "type": "pane", "pane": pane(value) } })
}

pub fn pane_read(output: PaneOutput) -> Value {
    let mut read = json!({
        "output": output.text,
        "revision": output.revision,
    });
    if let Some(range) = output.range {
        read["range"] = json!({
            "start": range.start,
            "end": range.end,
            "total": range.total,
        });
    }
    json!({ "result": { "type": "pane_read", "read": read } })
}

pub fn workspace_created(value: Workspace, created_tab: Tab, root_pane: Pane) -> Value {
    json!({
        "result": {
            "type": "workspace_created",
            "workspace": workspace(value),
            "tab": tab(created_tab),
            "root_pane": pane(root_pane),
        }
    })
}

pub fn tab_created(value: Tab, root_pane: Pane) -> Value {
    json!({
        "result": {
            "type": "tab_created",
            "tab": tab(value),
            "root_pane": pane(root_pane),
        }
    })
}

pub fn pane_created(value: Pane) -> Value {
    json!({ "result": { "type": "pane_created", "pane": pane(value) } })
}

pub fn command_ok(operation: &str) -> Value {
    json!({ "result": { "type": operation, "ok": true } })
}

pub fn snapshot(workspaces: Vec<Workspace>, tabs: Vec<Tab>, panes: Vec<Pane>) -> Value {
    let agents = panes.iter().filter_map(agent_from_pane).collect::<Vec<_>>();
    json!({
        "result": {
            "type": "session_snapshot",
            "workspaces": workspaces.into_iter().map(workspace).collect::<Vec<_>>(),
            "tabs": tabs.into_iter().map(tab).collect::<Vec<_>>(),
            "panes": panes.into_iter().map(pane).collect::<Vec<_>>(),
            "agents": agents,
        }
    })
}

pub fn agent_list(agents: &[Agent]) -> Value {
    json!({
        "result": {
            "type": "agent_list",
            "agents": agents.iter().map(agent).collect::<Vec<_>>()
        }
    })
}

pub fn agent_get(value: Agent) -> Value {
    json!({ "result": { "type": "agent", "agent": agent(&value) } })
}

fn workspace(value: Workspace) -> Value {
    json!({
        "workspace_id": value.id.as_str(),
        "number": value.number,
        "label": value.label,
        "focused": value.focused,
        "active_tab_id": value.active_tab_id.map(|id| id.as_str().to_owned()),
        "tab_count": value.tab_count,
        "pane_count": value.pane_count,
        "agent_status": agent_status(value.agent_status),
        "worktree": if value.repo_root.is_some() || value.checkout_path.is_some() {
            Some(json!({
                "repo_root": value.repo_root,
                "checkout_path": value.checkout_path,
            }))
        } else {
            None
        },
    })
}

fn tab(value: Tab) -> Value {
    json!({
        "tab_id": value.id.as_str(),
        "workspace_id": value.workspace_id.as_str(),
        "label": value.label,
        "focused": value.focused,
        "active_pane_id": value.active_pane_id.map(|id| id.as_str().to_owned()),
        "pane_count": value.pane_count,
    })
}

fn pane(value: Pane) -> Value {
    json!({
        "pane_id": value.id.as_str(),
        "terminal_id": value.terminal_id.as_deref().unwrap_or(value.id.as_str()),
        "workspace_id": value.workspace_id.as_str(),
        "tab_id": value.tab_id.as_str(),
        "label": value.label,
        "terminal_title_stripped": value.terminal_title,
        "cwd": value.cwd,
        "foreground_cwd": value.cwd,
        "focused": value.focused,
        "width": value.width,
        "height": value.height,
        "revision": value.revision,
        "foreground_command": value.foreground_command,
        "agent": value.agent,
        "agent_status": agent_status(value.agent_status),
        "scroll": {
            "max_offset_from_bottom": value.max_offset_from_bottom.unwrap_or(0),
            "viewport_rows": value.viewport_rows.or(value.height),
            // Gateway-internal, and currently unread (see `Pane::alternate_on`'s
            // own doc: card #795 found it could not tell an editor's pane from
            // an agent's). Left on this envelope regardless, since
            // `ScrollbackStore::observe` already walks it for every pane on
            // every list/snapshot and removing it buys nothing. Not consumed
            // by the app yet.
            "alternate_on": value.alternate_on,
        }
    })
}

fn agent_from_pane(value: &Pane) -> Option<Value> {
    let agent = value.agent.as_deref()?;
    Some(json!({
        "pane_id": value.id.as_str(),
        "workspace_id": value.workspace_id.as_str(),
        "tab_id": value.tab_id.as_str(),
        "agent": agent,
        "display_agent": agent,
        "agent_status": agent_status(value.agent_status),
    }))
}

fn agent(value: &Agent) -> Value {
    json!({
        "target": value.target,
        "pane_id": value.pane_id.as_str(),
        "workspace_id": value.workspace_id.as_ref().map(|id| id.as_str()),
        "tab_id": value.tab_id.as_ref().map(|id| id.as_str()),
        "agent": value.kind,
        "display_agent": value.display_agent.as_ref().or(value.kind.as_ref()),
        "agent_status": agent_status(value.status),
        "state_change_seq": value.state_change_seq,
    })
}

fn agent_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "starting",
        AgentStatus::Working => "working",
        AgentStatus::Idle => "idle",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Completed => "done",
        AgentStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::backend::{PaneId, TabId, WorkspaceId};

    #[test]
    fn tmux_entities_keep_the_existing_mobile_envelope() {
        let response = pane_list(vec![Pane {
            id: PaneId::new("%9"),
            terminal_id: Some("%9".into()),
            workspace_id: WorkspaceId::new("$0"),
            tab_id: TabId::new("@2"),
            label: Some("agent".into()),
            terminal_title: Some("agent".into()),
            cwd: Some(PathBuf::from("/work/project")),
            focused: true,
            width: Some(120),
            height: Some(40),
            revision: None,
            foreground_command: Some("claude".into()),
            agent: Some("claude".into()),
            agent_status: AgentStatus::Unknown,
            max_offset_from_bottom: Some(42),
            viewport_rows: Some(40),
            alternate_on: Some(true),
        }]);

        assert_eq!(response["result"]["type"], "pane_list");
        assert_eq!(response["result"]["panes"][0]["pane_id"], "%9");
        assert_eq!(response["result"]["panes"][0]["workspace_id"], "$0");
        assert_eq!(response["result"]["panes"][0]["agent"], "claude");
    }

    #[test]
    fn creation_envelopes_include_the_initial_terminal() {
        let pane = Pane {
            id: PaneId::new("%1"),
            terminal_id: Some("%1".into()),
            workspace_id: WorkspaceId::new("$1"),
            tab_id: TabId::new("@1"),
            label: None,
            terminal_title: None,
            cwd: None,
            focused: false,
            width: Some(80),
            height: Some(24),
            revision: None,
            foreground_command: Some("bash".into()),
            agent: None,
            agent_status: AgentStatus::Unknown,
            max_offset_from_bottom: Some(0),
            viewport_rows: Some(24),
            alternate_on: Some(false),
        };
        let tab = Tab {
            id: TabId::new("@1"),
            workspace_id: WorkspaceId::new("$1"),
            label: "shell".into(),
            focused: false,
            active_pane_id: Some(pane.id.clone()),
            pane_count: Some(1),
        };
        let workspace = Workspace {
            id: WorkspaceId::new("$1"),
            number: None,
            label: "work".into(),
            focused: false,
            active_tab_id: Some(tab.id.clone()),
            tab_count: Some(1),
            pane_count: Some(1),
            agent_status: AgentStatus::Unknown,
            repo_root: None,
            checkout_path: None,
        };

        let created = workspace_created(workspace, tab.clone(), pane.clone());
        assert_eq!(created["result"]["tab"]["tab_id"], "@1");
        assert_eq!(created["result"]["root_pane"]["pane_id"], "%1");
        let created = tab_created(tab, pane);
        assert_eq!(created["result"]["root_pane"]["pane_id"], "%1");
    }
}
