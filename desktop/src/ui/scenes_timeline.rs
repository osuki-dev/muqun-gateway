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

/// A subagent's session open in the detail sheet over its parent.
fn timeline_subagent(s: &mut UiState) {
    cards_state(s, false);
    let child: Vec<TimelineItem> = every_card().into_iter().take(9).collect();
    let mut tl = super::timeline::SessionTimeline::default();
    tl.replace(child);
    tl.earlier_exhausted = true;
    s.timelines.insert("s2".into(), tl);
    s.detail = Some(super::timeline::DetailSheet {
        stack: vec!["s2".into()],
        error: None,
    });
}

/// The context meter's sheet, with a Codex-style account limits answer.
fn timeline_context(s: &mut UiState) {
    cards_state(s, false);
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == "s1") {
        info.context_usage = serde_json::from_value(json!({
            "tokens": { "input": 820, "output": 1190, "cache_read": 174208, "cache_write": 0, "reasoning": 310 },
            "total_tokens": 211_400, "limit": { "context": 258_400 },
        }))
        .ok();
    }
    if let Some(agent) = s.agents.iter_mut().find(|a| a.id == "codex") {
        agent.features.extra.insert("accountLimits".into(), json!(true));
    }
    let soon = |mins: u64| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 + mins * 60_000)
            .unwrap_or(0)
    };
    s.account_limits.insert(
        "codex".into(),
        Some(json!({
            "status": "available", "reason": null, "observed_at_ms": 0, "ordinary_usage_allowed": true,
            "limits": [{ "id": "codex", "name": null, "windows": [
                { "id": "primary", "label": null, "unit": "percent", "used": 62.0, "limit": 100.0,
                  "remaining": 38.0, "window_duration_minutes": 300, "resets_at_ms": soon(134) },
                { "id": "secondary", "label": null, "unit": "percent", "used": 21.0, "limit": 100.0,
                  "remaining": 79.0, "window_duration_minutes": 10080, "resets_at_ms": soon(4 * 1440 + 200) },
            ] }],
        })),
    );
    s.context_open = Some("s1".into());
}

// ---- desktop timeline: folded activity, the detail pane, pictures ----------

/// A picture asset as the gateway attaches it to a row.
fn scene_asset(src: &str) -> muqun_gateway::agents::MessageImageAsset {
    serde_json::from_value(json!({
        "asset_id": "demo", "src": src, "url": src, "mime": "image/png",
        "width": 1024, "height": 1024,
    }))
    .expect("demo asset")
}

/// One realistic turn: a pasted screenshot (Codex's `[Image #1]`), a run of
/// reads and searches, words with code, edits and a failed check, and a
/// picture the agent made.
fn turn_items(running: bool) -> Vec<TimelineItem> {
    use TimelineRole::{Assistant, User};
    let sh = |id: &str, cmd: &str, out: &str, exit: i64, ms: u64| {
        tool_item(id, "exec_command", if exit == 0 { "completed" } else { "failed" }, json!({
            "input": { "command": cmd, "workdir": "/home/demo/gateway" },
            "output": out,
            "metadata": { "exit": exit },
            "time": { "ran": 1000, "completed": 1000 + ms },
        }))
    };
    let read = |id: &str, path: &str| {
        tool_item(id, "Read", "completed", json!({
            "input": { "file_path": path },
            "content": [{ "type": "text", "text": READ_OUT }],
        }))
    };
    let mut pasted = item("u1", User, AgentPart::Text {
        text: "[Image #1] The order tabs should show a count badge for each state.".into(),
    });
    pasted.attachments = Some(vec![SCENE_PICTURE.into()]);
    pasted.image_assets = Some(vec![scene_asset(SCENE_PICTURE)]);
    let mut made = item("a3img", Assistant, AgentPart::Text {
        text: format!("![](<file://{SCENE_PICTURE}>)"),
    });
    made.image_assets = Some(vec![scene_asset(SCENE_PICTURE)]);
    let mut items = vec![
        pasted,
        item("a0", Assistant, AgentPart::Text {
            text: "I'll find where the tabs are built and where the counts come from.".into(),
        }),
        item("r1", Assistant, AgentPart::Reasoning {
            text: "**Locating the tab bar**\n\nThe tabs live in the orders page; counts need a query per state.".into(),
            duration_ms: Some(3100),
        }),
        read("rd1", "/home/demo/shop/src/pages/orders/index.tsx"),
        read("rd2", "/home/demo/shop/src/pages/orders/tabs.tsx"),
        tool_item("gr1", "grep", "completed", json!({
            "input": { "pattern": "orderStatus", "path": "src" },
            "output": "Found 2 matches\n/home/demo/shop/src/api/orders.ts:\n  Line 12: export function orderStatus(\n/home/demo/shop/src/pages/orders/tabs.tsx:\n  Line 4: import { orderStatus } from '../../api/orders';",
        })),
        read("rd3", "/home/demo/shop/src/api/orders.ts"),
        sh("sh1", "rg -n \"status\" src/api | head -40", "src/api/orders.ts:12: export function orderStatus(\nsrc/api/orders.ts:31:   status: OrderState,", 0, 220),
        read("rd4", "/home/demo/shop/src/components/Badge.tsx"),
        item("a1", Assistant, AgentPart::Text {
            text: "The tabs render from a static list, so I'll add a `counts` query and show a badge only when a count is above zero:\n\n```tsx\nconst counts = useOrderCounts();\n<Tab label={t.label} badge={counts[t.state] || undefined} />\n```".into(),
        }),
        tool_item("ed1", "apply_patch", "completed", json!({
            "input": { "patchText": "*** Begin Patch\n*** Update File: src/pages/orders/tabs.tsx\n@@\n-  <Tab label={t.label} />\n+  <Tab label={t.label} badge={counts[t.state] || undefined} />\n*** End Patch" },
        })),
        tool_item("ed2", "edit", "completed", json!({
            "input": { "file_path": "src/api/orders.ts" },
            "metadata": { "files": [{ "file": "src/api/orders.ts", "patch": PATCH }] },
        })),
        sh("sh2", "pnpm tsc --noEmit", "src/pages/orders/tabs.tsx(18,7): error TS2322: Type 'number | undefined' is not assignable to type 'string'.\n", 2, 6400),
        tool_item("ed3", "edit", "completed", json!({
            "input": { "file_path": "src/components/Badge.tsx" },
            "metadata": { "files": [{ "file": "src/components/Badge.tsx", "patch": PATCH }] },
        })),
        sh("sh3", "pnpm tsc --noEmit", "", 0, 5900),
        sh("sh4", "pnpm test --run orders", "✓ orders/tabs.test.tsx (4 tests) 41ms\n\nTest Files  1 passed (1)\n     Tests  4 passed (4)", 0, 3200),
        item("a2", Assistant, AgentPart::Text {
            text: "Each tab now shows how many orders are in that state; the badge hides at zero. Here is a mock of the result:".into(),
        }),
        made,
    ];
    if running {
        items.truncate(items.len() - 2);
        items.push(tool_item("sh5", "exec_command", "running", json!({
            "input": { "command": "pnpm test --run orders --coverage" },
            "output": " RUN  v2.1.3 /home/demo/shop",
        })));
    }
    items
}

fn turn_state(s: &mut UiState, running: bool) {
    base(s);
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == "s1") {
        info.status = if running {
            muqun_gateway::agents::AgentSessionStatus::Busy
        } else {
            muqun_gateway::agents::AgentSessionStatus::Idle
        };
    }
    s.apply_updates(vec![crate::model::inbox::Update::Snapshot(Box::new(
        muqun_gateway::agents::ports::AgentSessionSnapshot {
            info: s.sessions[0].clone(),
            timeline: turn_items(running),
            permissions: vec![],
            forms: vec![],
            inbox: vec![],
            seq: 1,
        },
    ))]);
    if let Some(tl) = s.timelines.get_mut("s1") {
        tl.earlier_exhausted = true;
    }
    s.mode = Mode::Session;
    s.right_open = false;
    s.right.set_open(false);
    s.demo = true;
}

/// Activity folded under its summary lines.
fn timeline_grouped(s: &mut UiState) {
    turn_state(s, false);
}

/// The second group open, a row selected.
fn timeline_expanded(s: &mut UiState) {
    turn_state(s, false);
    if let Some(tl) = s.timelines.get_mut("s1") {
        tl.flip("grp-ed1");
        tl.selected = Some("sh2".into());
    }
}

/// The agent at work: the running group names its step.
fn timeline_running(s: &mut UiState) {
    turn_state(s, true);
}

/// The failed check open in the detail pane.
fn timeline_detail(s: &mut UiState) {
    timeline_expanded(s);
    s.inspect = Some(super::Inspect {
        asid: "s1".into(),
        id: "sh2".into(),
    });
    s.inspect_pane.set_open(true);
}
