//! Serialization at the legacy HTTP boundary.
//!
//! The mobile app predates the backend abstraction and consumes the Herdr
//! response envelope. Keeping that compatibility here prevents either backend
//! from shaping itself around an HTTP DTO.

use serde_json::{json, Value};

use super::{AgentStatus, Pane, PaneOutput, Tab, Workspace};

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
    json!({
        "result": {
            "type": "pane_read",
            "read": {
                "output": output.text,
                "revision": output.revision,
            }
        }
    })
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
    let agents = panes.iter().filter_map(agent).collect::<Vec<_>>();
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

pub fn agent_list(panes: &[Pane]) -> Value {
    json!({
        "result": {
            "type": "agent_list",
            "agents": panes.iter().filter_map(agent).collect::<Vec<_>>()
        }
    })
}

fn workspace(value: Workspace) -> Value {
    json!({
        "workspace_id": value.id.as_str(),
        "label": value.label,
        "focused": value.focused,
        "active_tab_id": value.active_tab_id.map(|id| id.as_str().to_owned()),
        "tab_count": value.tab_count,
        "agent_status": "unknown",
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
        "terminal_id": value.id.as_str(),
        "workspace_id": value.workspace_id.as_str(),
        "tab_id": value.tab_id.as_str(),
        "label": value.label,
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
            "max_offset_from_bottom": if value.has_scrollback { 1 } else { 0 },
            "viewport_rows": value.height,
        }
    })
}

fn agent(value: &Pane) -> Option<Value> {
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

fn agent_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => "starting",
        AgentStatus::Working => "working",
        AgentStatus::Idle => "idle",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Completed => "completed",
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
            workspace_id: WorkspaceId::new("$0"),
            tab_id: TabId::new("@2"),
            label: Some("agent".into()),
            cwd: Some(PathBuf::from("/work/project")),
            focused: true,
            width: Some(120),
            height: Some(40),
            revision: None,
            foreground_command: Some("claude".into()),
            agent: Some("claude".into()),
            agent_status: AgentStatus::Unknown,
            has_scrollback: true,
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
            workspace_id: WorkspaceId::new("$1"),
            tab_id: TabId::new("@1"),
            label: None,
            cwd: None,
            focused: false,
            width: Some(80),
            height: Some(24),
            revision: None,
            foreground_command: Some("bash".into()),
            agent: None,
            agent_status: AgentStatus::Unknown,
            has_scrollback: false,
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
            label: "work".into(),
            focused: false,
            active_tab_id: Some(tab.id.clone()),
            tab_count: Some(1),
        };

        let created = workspace_created(workspace, tab.clone(), pane.clone());
        assert_eq!(created["result"]["tab"]["tab_id"], "@1");
        assert_eq!(created["result"]["root_pane"]["pane_id"], "%1");
        let created = tab_created(tab, pane);
        assert_eq!(created["result"]["root_pane"]["pane_id"], "%1");
    }
}
