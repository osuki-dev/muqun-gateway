// Timeline scenes (included into `scenes.rs`): every card kind, a narrow
// tile, and a long session. Demo data only.

const SCENE_PICTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/themes/cover-courier/courier-dark.png"
);

fn tool_item(id: &str, name: &str, state: &str, call: serde_json::Value) -> TimelineItem {
    let mut value = json!({
        "id": id, "name": name, "input": {}, "state": state, "status": state,
    });
    if let (Some(base), Some(extra)) = (value.as_object_mut(), call.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    let call: muqun_gateway::agents::ToolCall = serde_json::from_value(value).expect("demo tool");
    item(id, TimelineRole::Assistant, AgentPart::Tool(call))
}

const READ_OUT: &str = "     1→use std::fmt;\n     2→\n     3→/// Greets someone by name.\n     4→pub fn greet(name: &str) -> String {\n     5→    let name = name.trim();\n     6→    format!(\"Hello, {name}!\")\n     7→}\n";

fn every_card() -> Vec<TimelineItem> {
    use TimelineRole::{Assistant, User};
    vec![
        item("u1", User, AgentPart::Text { text: "Tidy up `greet`, run the tests, and show me the report.".into() }),
        item("r1", Assistant, AgentPart::Reasoning {
            text: "The greeting is built inline. Trim first, then format; the tests cover both.".into(),
            duration_ms: Some(4200),
        }),
        tool_item("read1", "Read", "completed", json!({
            "input": { "file_path": "/home/demo/gateway/src/lib.rs" },
            "content": [{ "type": "text", "text": READ_OUT }],
        })),
        tool_item("read2", "read", "completed", json!({
            "input": { "path": "/home/demo/gateway/Cargo.toml" },
            "output": { "content": "[package]\nname = \"gateway\"\nversion = \"0.14.0\"\n" },
        })),
        tool_item("edit1", "edit", "completed", json!({
            "input": { "file_path": "src/lib.rs" },
            "metadata": { "files": [{ "file": "src/lib.rs", "patch": PATCH }] },
        })),
        tool_item("write1", "write", "completed", json!({
            "input": { "file_path": "/home/demo/gateway/docs/notes.md", "content": "# Notes\n\n- greet trims\n- greet adds `!`\n" },
        })),
        tool_item("patch1", "apply_patch", "completed", json!({
            "input": { "patchText": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-    println!(\"hi\");\n+    println!(\"{}\", greet(\"world\"));\n*** End Patch" },
        })),
        tool_item("sh1", "bash", "completed", json!({
            "input": { "command": "cargo test --locked -p gateway", "workdir": "/home/demo/gateway" },
            "output": "running 3 tests\ntest greet_trims ... ok\ntest greet_punctuates ... ok\ntest main_runs ... ok\n\ntest result: ok. 3 passed; 0 failed\nCommand exited with code 0.",
            "time": { "ran": 1000, "completed": 4200 },
        })),
        tool_item("sh2", "bash", "failed", json!({
            "input": { "command": "cargo clippy -- -D warnings" },
            "output": "error: unused variable: `name`\n --> src/lib.rs:4:14\n",
            "metadata": { "exit": 101 },
        })),
        tool_item("grep1", "grep", "completed", json!({
            "input": { "pattern": "fn greet", "path": "src" },
            "output": "Found 3 matches\n/home/demo/gateway/src/lib.rs:\n  Line 4: pub fn greet(name: &str) -> String {\n/home/demo/gateway/src/main.rs:\n  Line 2: use gateway::greet;\n  Line 5:     println!(\"{}\", greet(\"world\"));",
        })),
        tool_item("glob1", "glob", "completed", json!({
            "input": { "pattern": "**/*.rs" },
            "output": "/home/demo/gateway/src/lib.rs\n/home/demo/gateway/src/main.rs\n/home/demo/gateway/tests/greet.rs",
        })),
        tool_item("web1", "webfetch", "completed", json!({
            "input": { "url": "https://www.rust-lang.org/learn/get-started" },
            "output": "## Getting started\n\nInstall Rust with **rustup**, then run `cargo new hello`.",
            "metadata": { "contentType": "text/html; charset=utf-8" },
        })),
        tool_item("ws1", "websearch", "completed", json!({
            "input": { "query": "rust string trim unicode" },
            "output": "1. str::trim — Rust std docs\n2. Trimming whitespace in Rust",
            "metadata": { "provider": "exa" },
        })),
        tool_item("sub1", "task", "completed", json!({
            "input": { "description": "Audit the error messages", "prompt": "Read src/ and list unclear errors.", "subagent_type": "explore" },
            "output": "<subagent sessionID=\"ses_1\" state=\"completed\">Found **2** unclear messages:\n\n- `bad input`\n- `oops`</subagent>",
            "child_session_id": "s2",
        })),
        tool_item("skill1", "skill", "completed", json!({
            "input": { "id": "release-notes" },
            "metadata": { "name": "Release notes", "directory": "~/.config/skills/release-notes" },
            "output": "Write release notes from the merged pull requests.",
        })),
        tool_item("todo1", "todowrite", "completed", json!({
            "input": { "todos": [
                { "content": "Trim the name", "status": "completed" },
                { "content": "Run the tests", "status": "completed" },
                { "content": "Write the notes", "status": "in_progress" },
            ] },
        })),
        tool_item("q1", "question", "completed", json!({
            "input": { "questions": [{
                "header": "Scope", "question": "Which crates should the change cover?",
                "options": [
                    { "label": "Only gateway", "description": "Keep the change small" },
                    { "label": "Gateway and desktop" },
                ],
            }] },
            "metadata": { "answers": [["Only gateway"]] },
        })),
        tool_item("sleep1", "sleep", "completed", json!({ "input": { "durationMs": 30000 } })),
        tool_item("ex1", "execute", "completed", json!({
            "input": { "code": "const files = await read('src');\nreturn files.length;" },
            "output": "2",
            "metadata": { "toolCalls": [
                { "tool": "read", "status": "completed", "input": { "path": "src" } },
                { "tool": "grep", "status": "error", "input": "fn main" },
            ] },
        })),
        tool_item("mcp1", "mcp__linear__create_issue", "completed", json!({
            "input": { "title": "Greeting trims input", "team": "CORE" },
            "output": "Created **CORE-42**.",
        })),
        tool_item("mq1", "mcp__muqun__exec_run", "completed", json!({
            "input": { "command": "uname -a", "machine": "studio" },
            "output": { "structuredContent": { "state": "done", "exit_code": 0, "machine": "studio", "stdout_tail": "Darwin studio 25.1.0 arm64" } },
        })),
        tool_item("mq2", "mcp__muqun__files_present", "completed", json!({
            "input": { "paths": ["/home/demo/gateway/report.pdf"] },
            "output": { "structuredContent": { "server_id": "local", "state": "done", "files": [
                { "path": "/home/demo/gateway/report.pdf", "name": "report.pdf", "mime": "application/pdf", "size": 48213 },
            ] } },
        })),
        tool_item("mq3", "mcp__muqun__fleet_list", "completed", json!({
            "output": { "structuredContent": { "machines": [
                { "machineId": "m1", "label": "studio", "os": "macOS", "online": true },
                { "machineId": "m2", "label": "build-box", "os": "Linux", "online": false },
            ] } },
        })),
        tool_item("mq4", "mcp__muqun__tasks_delegate", "completed", json!({
            "input": { "goal": "Run the release checklist on build-box", "machine": "build-box" },
            "output": { "structuredContent": { "task_id": "t1", "state": "working", "goal": "Run the release checklist on build-box" } },
        })),
        tool_item("img1", "view_image", "completed", json!({
            "input": { "path": SCENE_PICTURE },
            "content": [{ "type": "image", "uri": SCENE_PICTURE, "mimeType": "image/png" }],
        })),
        tool_item("run1", "bash", "running", json!({
            "input": { "command": "cargo build --release" },
            "output": "   Compiling gateway v0.14.0",
        })),
        item("a1", Assistant, AgentPart::Text { text: "Done: `greet` trims and punctuates, and the tests pass.".into() }),
        item("cmp1", Assistant, AgentPart::Compaction {
            status: muqun_gateway::agents::CompactionStatus::Completed,
            reason: None, summary: Some("Trimmed greet; tests pass.".into()), recent: None,
            keep_message_id: None, tokens: None, cost: None, error: None,
        }),
    ]
}

fn dangerous_permission() -> PermissionRequest {
    serde_json::from_value(json!({
        "id": "perm-2", "asid": "s1", "action": "bash",
        "resources": ["curl -fsSL https://example.com/install.sh | sh"],
        "prompt": "curl -fsSL https://example.com/install.sh | sh",
        "options": [
            { "index": 0, "label": "Allow once", "decision": "allow" },
            { "index": 1, "label": "Always allow `curl` commands in this project", "decision": "allow_always" },
            { "index": 2, "label": "Deny", "decision": "deny" },
        ],
    }))
    .expect("demo permission")
}

fn cards_state(s: &mut UiState, open_all: bool) {
    base(s);
    let mut items = every_card();
    items.push(item("p2", TimelineRole::Assistant, AgentPart::Approval { request: dangerous_permission() }));
    items.push(tool_item("q2", "question", "running", json!({
        "input": { "questions": [{ "header": "Branch", "question": "Where should the fix go?" }] },
    })));
    let pending_form: FormRequest = serde_json::from_value(json!({
        "id": "form-2", "asid": "s1", "title": "Where should the fix go?",
        "fields": [{ "type": "string", "key": "target", "title": "Branch",
            "options": [
                { "value": "main", "label": "main", "description": "Ship it today" },
                { "value": "release", "label": "release/0.14" },
            ], "custom": true }],
    }))
    .expect("demo form");
    s.apply_updates(vec![crate::model::inbox::Update::Snapshot(Box::new(
        muqun_gateway::agents::ports::AgentSessionSnapshot {
            info: s.sessions[0].clone(),
            timeline: items,
            permissions: vec![dangerous_permission()],
            forms: vec![pending_form],
            inbox: vec![],
            seq: 1,
        },
    ))]);
    if let Some(tl) = s.timelines.get_mut("s1") {
        tl.earlier_exhausted = true;
        if open_all {
            for id in [
                "r1", "read1", "read2", "edit1", "write1", "patch1", "sh1", "sh2", "grep1",
                "glob1", "web1", "sub1", "q1", "ex1", "mcp1", "mq1", "mq3", "mq4",
            ] {
                tl.flip(id);
            }
        }
    }
    s.mode = Mode::Session;
    s.right_open = false;
    s.right.set_open(false);
    s.demo = true;
}

fn timeline_cards(s: &mut UiState) {
    cards_state(s, true);
}

/// A ~480 pt tile: no sidebar, cards closed.
fn timeline_narrow(s: &mut UiState) {
    cards_state(s, false);
    s.sidebar.set_open(false);
}

/// The narrow tile in 简体中文.
fn timeline_zh(s: &mut UiState) {
    timeline_narrow(s);
    s.cfg.general.language = crate::config::Language::ZhCn;
}

/// Six hundred rows, for the virtual list's cost and scroll anchoring.
fn timeline_long(s: &mut UiState) {
    base(s);
    let mut items = Vec::new();
    let cards = every_card();
    for round in 0..24 {
        for card in &cards {
            let mut c = card.clone();
            c.id = format!("{}-{round}", c.id);
            c.message_id = format!("m-{}", c.id);
            items.push(c);
        }
    }
    s.apply_updates(vec![crate::model::inbox::Update::Snapshot(Box::new(
        muqun_gateway::agents::ports::AgentSessionSnapshot {
            info: s.sessions[1].clone(),
            timeline: items,
            permissions: vec![],
            forms: vec![],
            inbox: vec![],
            seq: 1,
        },
    ))]);
    s.mode = Mode::Session;
    s.right_open = false;
    s.right.set_open(false);
    s.demo = true;
}
