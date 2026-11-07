//! Named states for `AppSpec::scenes`: demo data so screenshots need no live gateway.

use muqun_gateway::agents::{
    AgentPart, AgentSessionInfo, FormRequest, PermissionRequest, TimelineItem, TimelineRole,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use rune::TextEdit;
use rune_kit::diff::{parse, DiffState};
use serde_json::json;

use super::{InspectorTab, Mode, SettingsTab, UiState};
use crate::hub::TaskRow;
use crate::model::compose::InboxItem;
use crate::model::types::{DiffFile, DiffStat};

const PATCH: &str = "diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,6 +1,8 @@
 pub fn greet(name: &str) -> String {
-    format!(\"hi {name}\")
+    let name = name.trim();
+    format!(\"Hello, {name}!\")
 }
 
 pub fn main() {
     println!(\"{}\", greet(\"world\"));
+    println!(\"{}\", greet(\"  rune \"));
 }
";

fn session(
    asid: &str,
    title: &str,
    agent: &str,
    status: &str,
    dir: &str,
    age: u64,
) -> AgentSessionInfo {
    serde_json::from_value(json!({
        "asid": asid, "agent_id": agent, "title": title, "status": status,
        "directory": dir, "updated_ms": 1_800_000_000_000u64 - age,
        "model": { "provider_id": "anthropic", "model_id": "sonnet" },
    }))
    .expect("demo session")
}

fn agent(id: &str, name: &str, enabled: bool) -> AgentDiscoveryInfo {
    let features: serde_json::Value = [
        "streaming",
        "reasoningEffort",
        "modelSelection",
        "toolApprovals",
        "worktrees",
        "revert",
        "stagedRevert",
        "inbox",
        "modes",
        "skills",
        "slashCommands",
        "compaction",
        "backgroundShells",
        "attachments",
        "attachmentsByPath",
    ]
    .iter()
    .map(|k| (k.to_string(), json!(false)))
    .collect::<serde_json::Map<_, _>>()
    .into();
    serde_json::from_value(json!({
        "id": id, "name": name, "kind": id, "status": if enabled { "connected" } else { "disabled" },
        "enabled": enabled, "models": [], "modes": [], "features": features,
    }))
    .expect("demo agent")
}

fn item(id: &str, role: TimelineRole, part: AgentPart) -> TimelineItem {
    TimelineItem {
        id: id.into(),
        message_id: format!("m-{id}"),
        role,
        part,
        seq: 0,
        updated_ms: 0,
        ordinal: 0,
        attachments: None,
        image_assets: None,
        generation: None,
        origin: None,
    }
}

fn base(s: &mut UiState) {
    s.hub_label = "standalone".into();
    s.agents_seen = true;
    s.splash = None;
    s.status = "Demo".into();
    s.agents = vec![
        agent("codex", "Codex", true),
        agent("opencode", "OpenCode", true),
        agent("pi", "Pi", false),
    ];
    s.start_agent = "codex".into();
    s.start_cwd = TextEdit::new("/home/demo/gateway");
    s.sessions = vec![
        session(
            "s1",
            "Fix the greeting",
            "codex",
            "busy",
            "/home/demo/gateway",
            1_000,
        ),
        session(
            "s2",
            "Review the release notes",
            "opencode",
            "idle",
            "/home/demo/gateway",
            60_000,
        ),
        session(
            "s3",
            "Port the settings page",
            "codex",
            "idle",
            "/home/demo/rune",
            3_600_000,
        ),
    ];
    s.selected = Some("s1".into());
}

fn permission() -> PermissionRequest {
    serde_json::from_value(json!({
        "id": "perm-1", "asid": "s1", "action": "bash", "resources": ["cargo test --locked"],
        "prompt": "Run the test suite?", "options": [],
    }))
    .expect("demo permission")
}

fn form() -> FormRequest {
    serde_json::from_value(json!({
        "id": "form-1", "asid": "s1", "title": "Pick a target",
        "fields": [
            { "type": "string", "key": "branch", "title": "Branch", "placeholder": "main" },
            { "type": "boolean", "key": "push", "title": "Push when done", "default": false },
        ],
    }))
    .expect("demo form")
}

fn timeline(s: &mut UiState) {
    use TimelineRole::{Assistant, User};
    let call: muqun_gateway::agents::ToolCall = serde_json::from_value(json!({
        "id": "t1", "name": "edit", "title": "Edit src/lib.rs", "input": { "file_path": "src/lib.rs" },
        "output": "Applied 1 edit.", "metadata": { "files": [{ "file": "src/lib.rs", "patch": PATCH }] },
        "state": "completed", "status": "completed",
    }))
    .expect("demo tool");
    let items = vec![
        item("u1", User, AgentPart::Text { text: "Make `greet` trim its argument and add a trailing `!`.".into() }),
        item("r1", Assistant, AgentPart::Reasoning { text: "The function formats directly; trim first.".into(), duration_ms: Some(2300) }),
        item("a1", Assistant, AgentPart::Text { text: "I'll update **`greet`** and run the tests.\n\n- trim the input\n- add punctuation".into() }),
        item("t1", Assistant, AgentPart::Tool(call)),
        item("d1", Assistant, AgentPart::Todo { items: vec![
            muqun_gateway::agents::TodoItem { text: "Edit greet".into(), done: true },
            muqun_gateway::agents::TodoItem { text: "Run cargo test".into(), done: false },
        ] }),
        item("p1", Assistant, AgentPart::Approval { request: permission() }),
        item("f1", Assistant, AgentPart::Form { request: form() }),
        item("st1", Assistant, AgentPart::Status { text: "Waiting for approval".into() }),
    ];
    s.apply_updates(vec![crate::model::inbox::Update::Snapshot(Box::new(
        muqun_gateway::agents::ports::AgentSessionSnapshot {
            info: s.sessions[0].clone(),
            timeline: items,
            permissions: vec![permission()],
            forms: vec![form()],
            inbox: vec![],
            seq: 1,
        },
    ))]);
    if let Some(tl) = s.timelines.get_mut("s1") {
        tl.flip("r1");
    }
    s.mode = Mode::Session;
}

fn diff_stat(s: &mut UiState) {
    s.diff_stat = DiffStat {
        asid: "s1".into(),
        files: 1,
        added: 3,
        removed: 1,
        paths: vec![DiffFile {
            path: "src/lib.rs".into(),
            added: 3,
            removed: 1,
            patch: PATCH.into(),
        }],
    };
    s.diff = Some(DiffState::new(parse(PATCH)));
}

fn home(s: &mut UiState) {
    base(s);
    s.selected = None;
    s.mode = Mode::Home;
    s.start_draft = TextEdit::new("Add a settings page that edits the agent list.");
    s.demo = true;
}

fn session_scene(s: &mut UiState) {
    base(s);
    timeline(s);
    diff_stat(s);
    s.queue = vec![InboxItem {
        id: "q1".into(),
        text: "Then update the changelog".into(),
        steer: false,
        files: Vec::new(),
    }];
    s.right_open = true;
    s.right.set_open(true);
    s.inspector_tab = InspectorTab::Queue;
    s.draft = TextEdit::new("");
    s.demo = true;
}

fn diff(s: &mut UiState) {
    base(s);
    timeline(s);
    diff_stat(s);
    s.show_diff = true;
    s.right_open = true;
    s.inspector_tab = InspectorTab::Git;
    s.demo = true;
}

fn settings(s: &mut UiState) {
    base(s);
    s.mode = Mode::Settings;
    s.settings_tab = SettingsTab::Agents;
    s.demo = true;
}

fn tasks(s: &mut UiState) {
    base(s);
    s.mode = Mode::Tasks;
    s.task_cwd = Some("/home/demo/gateway".into());
    s.tasks = vec![
        TaskRow {
            id: "k1".into(),
            state: "working".into(),
            title: "Audit the release workflow".into(),
            progress: Some("3 of 5 steps".into()),
            agent_id: Some("codex".into()),
            asid: Some("s1".into()),
            ..TaskRow::default()
        },
        TaskRow {
            id: "k2".into(),
            state: "waiting".into(),
            title: "Draft the migration notes".into(),
            origin: "device".into(),
            completable: true,
            ..TaskRow::default()
        },
        TaskRow {
            id: "k3".into(),
            state: "done".into(),
            title: "Bump dependencies".into(),
            ..TaskRow::default()
        },
    ];
    s.demo = true;
}

fn inbox(s: &mut UiState) {
    base(s);
    s.waiting_perms.insert("s1".into(), vec![permission()]);
    s.waiting_forms.insert("s1".into(), vec![form()]);
    s.mode = Mode::Inbox;
    s.demo = true;
}

include!("scenes_timeline.rs");

/// A scene name and the state it applies.
pub type Scene = (&'static str, fn(&mut UiState));

pub fn scenes() -> &'static [Scene] {
    &[
        ("home", home),
        ("session", session_scene),
        ("settings", settings),
        ("tasks", tasks),
        ("inbox", inbox),
        ("diff", diff),
        ("timeline-cards", timeline_cards),
        ("timeline-narrow", timeline_narrow),
        ("timeline-long", timeline_long),
        ("timeline-zh", timeline_zh),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;

    #[test]
    fn every_scene_builds_its_state() {
        for (name, scene) in scenes() {
            let (tx, _rx) = commands::channel();
            let mut state = UiState::new(tx);
            scene(&mut state);
            assert!(!state.agents.is_empty(), "{name}");
        }
    }
}
