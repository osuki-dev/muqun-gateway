//! Named states for `AppSpec::scenes`: demo data so screenshots need no live gateway.

use muqun_gateway::agents::{
    AgentPart, AgentSessionInfo, FormRequest, PermissionRequest, TimelineItem, TimelineRole,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use rune::TextEdit;
use rune_kit::diff::{parse, DiffState};
use serde_json::json;

use super::{InspectorTab, Mode, SessionSlot, SettingsTab, Tile, UiState};
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

/// Make a built-in theme pack current, so a scene renders in it.
fn with_pack(s: &mut UiState, id: &str) {
    let Some(pack) = crate::theme::store::builtin(id) else {
        return;
    };
    let slot = |slot: &str, dark: bool| {
        pack.manifest
            .slot_asset(slot, dark)
            .and_then(|asset| pack.asset(asset))
    };
    let applied = crate::theme::apply::AppliedTheme {
        manifest: std::sync::Arc::new(pack.manifest.clone()),
        scheme: crate::config::Scheme::System,
        wallpaper_light: slot("home.wallpaper", false),
        wallpaper_dark: slot("home.wallpaper", true),
        artwork_light: slot("home.artwork", false),
        artwork_dark: slot("home.artwork", true),
        launch_light: slot("launch.artwork", false).or_else(|| slot("home.artwork", false)),
        launch_dark: slot("launch.artwork", true).or_else(|| slot("home.artwork", true)),
        requested: std::time::Instant::now(),
    };
    if crate::theme::apply::activate(applied).is_ok() {
        s.cfg.appearance.theme_id = id.to_string();
    }
}

/// The launch splash, caught mid-hold.
fn splash_scene(s: &mut UiState) {
    base(s);
    s.mode = Mode::Home;
    s.selected = None;
    let mut splash = super::splash::Splash::start(super::splash::SplashKind::Boot);
    splash.started = std::time::Instant::now() - std::time::Duration::from_millis(1000);
    s.hub_label.clear();
    s.splash = Some(splash);
    s.demo = true;
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
    s.open_items.insert("r1".into());
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

/// Home while agents work: one approval waiting, one session running, a task.
fn home_busy(s: &mut UiState) {
    base(s);
    s.sessions.push(session(
        "s4",
        "Migrate the config loader",
        "opencode",
        "busy",
        "/home/demo/rune",
        5_000,
    ));
    s.waiting_perms.insert("s1".into(), vec![permission()]);
    s.tasks = vec![TaskRow {
        id: "k1".into(),
        state: "working".into(),
        title: "Audit the release workflow".into(),
        progress: Some("3 of 5 steps".into()),
        agent_id: Some("codex".into()),
        asid: Some("s1".into()),
        ..TaskRow::default()
    }];
    s.selected = None;
    s.mode = Mode::Home;
    s.demo = true;
}

/// Home before the first session: the welcome.
fn home_first(s: &mut UiState) {
    base(s);
    s.sessions.clear();
    s.cfg.general.last_agent = None;
    s.cfg.general.last_cwd.clear();
    s.selected = None;
    s.mode = Mode::Home;
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

/// A parked tile's own short timeline.
fn parked_items(prefix: &str, ask: &str, reply: &str) -> Vec<TimelineItem> {
    use TimelineRole::{Assistant, User};
    vec![
        item(
            &format!("{prefix}-u1"),
            User,
            AgentPart::Text { text: ask.into() },
        ),
        item(
            &format!("{prefix}-a1"),
            Assistant,
            AgentPart::Text { text: reply.into() },
        ),
    ]
}

/// Two sessions side by side: the greeting fix (focused) and the release
/// notes review streaming beside it.
fn split2(s: &mut UiState) {
    base(s);
    timeline(s);
    diff_stat(s);
    s.tiles = vec![
        Tile {
            asid: Some("s1".into()),
            slot: SessionSlot::default(),
        },
        Tile {
            asid: Some("s2".into()),
            slot: SessionSlot {
                timeline: parked_items(
                    "s2",
                    "Review the release notes for 3.1.0 and tighten the wording.",
                    "Reading `release-notes/unreleased.md`…\n\n- The **Pad layout** entry needs the keyboard shortcut.\n- Two entries repeat the PR number.",
                ),
                draft: TextEdit::new("Also check the Chinese copy"),
                ..SessionSlot::default()
            },
        },
    ];
    s.focus = 0;
    s.draft = TextEdit::new("");
    s.demo = true;
}

/// Four tiles: two working, one waiting on an approval, one empty picker.
fn split4(s: &mut UiState) {
    base(s);
    s.sessions.push(session(
        "s4",
        "Migrate the config loader",
        "opencode",
        "busy",
        "/home/demo/rune",
        5_000,
    ));
    timeline(s);
    s.tiles = vec![
        Tile {
            asid: Some("s1".into()),
            slot: SessionSlot::default(),
        },
        Tile {
            asid: Some("s2".into()),
            slot: SessionSlot {
                timeline: parked_items(
                    "s2",
                    "Review the release notes for 3.1.0.",
                    "Two entries repeat the PR number; fixing.",
                ),
                ..SessionSlot::default()
            },
        },
        Tile {
            asid: Some("s4".into()),
            slot: SessionSlot {
                timeline: parked_items(
                    "s4",
                    "Move the config loader onto serde defaults.",
                    "Working through `config.rs`: 3 of 7 structs done.",
                ),
                ..SessionSlot::default()
            },
        },
        Tile {
            asid: None,
            slot: SessionSlot::default(),
        },
    ];
    s.focus = 1;
    // Tile 1 is focused: its state lives in the top-level fields.
    let focused = std::mem::take(&mut s.tiles[1].slot);
    let s1 = SessionSlot {
        timeline: std::mem::take(&mut s.timeline),
        ..SessionSlot::default()
    };
    s.tiles[0].slot = s1;
    s.timeline = focused.timeline;
    s.selected = Some("s2".into());
    s.draft = TextEdit::new("");
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

const VIEWER_CODE: &str = r#"//! Retry a flaky request with capped exponential backoff.

use std::time::Duration;

/// How long to wait before attempt `n` (0-based): 200 ms, doubling, at most 5 s.
pub fn backoff(n: u32) -> Duration {
    let ms = 200u64.saturating_mul(1 << n.min(16));
    Duration::from_millis(ms.min(5_000))
}

pub async fn with_retry<T, E, F, Fut>(mut attempt: F, tries: u32) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut n = 0;
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(err) if n + 1 >= tries => return Err(err),
            Err(_) => {
                tokio::time::sleep(backoff(n)).await;
                n += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn backoff_is_capped() {
        assert_eq!(super::backoff(0).as_millis(), 200);
        assert_eq!(super::backoff(10).as_millis(), 5_000);
    }
}
"#;

const VIEWER_MARKDOWN: &str = "# Release checklist

Before tagging **v0.4.0**, walk this list top to bottom.

## Build

- [x] `cargo build --release` on Linux, macOS and Windows
- [x] Installer smoke test in a clean container
- [ ] Notarize the macOS bundle

## Notes

| Area | Owner | State |
|---|---|---|
| Desktop | platform | ready |
| Gateway | core | review |

```sh
desktop/scripts/install-local.sh
```

> Ship nothing that has not run on a real machine.
";

fn viewer_with(s: &mut UiState, name: &str, kind: crate::model::viewer::FileKind, body: &str) {
    use super::viewer::{text_body, Viewer};
    session_scene(s);
    s.viewer = Some(Viewer {
        path: Some(format!("/home/demo/gateway/{name}").into()),
        name: name.rsplit('/').next().unwrap_or(name).to_string(),
        kind,
        meta: crate::model::viewer::Meta {
            size: Some(body.len() as u64),
            modified: Some(std::time::SystemTime::now() - std::time::Duration::from_secs(720)),
        },
        body: text_body(body.into(), false),
        zoom: None,
        source: false,
    });
}

fn viewer_code(s: &mut UiState) {
    use crate::model::viewer::FileKind;
    viewer_with(s, "src/retry.rs", FileKind::Text(Some("rust")), VIEWER_CODE);
}

fn viewer_markdown(s: &mut UiState) {
    use crate::model::viewer::FileKind;
    viewer_with(s, "docs/RELEASE.md", FileKind::Markdown, VIEWER_MARKDOWN);
}

fn viewer_image(s: &mut UiState) {
    session_scene(s);
    s.open_file(std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/icons/app-icon.png"
    )));
}

fn viewer_video(s: &mut UiState) {
    use crate::model::viewer::FileKind;
    viewer_with(s, "recordings/demo-run.mp4", FileKind::Video, "");
    if let Some(v) = s.viewer.as_mut() {
        v.body = super::viewer::Body::External;
        v.meta.size = Some(48_734_112);
    }
}

/// A scene name and the state it applies.
pub type Scene = (&'static str, fn(&mut UiState));

pub fn scenes() -> &'static [Scene] {
    &[
        ("home", home),
        ("home-busy", home_busy),
        ("home-first", home_first),
        ("splash", splash_scene),
        ("home-plain", |s| {
            home_busy(s);
            s.cfg.appearance.reduced_motion = true;
            s.cfg.appearance.reduced_transparency = true;
        }),
        ("home-zh", |s| {
            home_busy(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("split2-zh", |s| {
            split2(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("home-akiba", |s| {
            with_pack(s, "akiba-overdrive");
            home_busy(s);
        }),
        ("home-jade", |s| {
            with_pack(s, "jade-tempest");
            home_busy(s);
        }),
        ("home-voyager", |s| {
            with_pack(s, "scene-voyager");
            home(s);
        }),
        ("home-eclipse", |s| {
            with_pack(s, "eclipse-archivist");
            home(s);
        }),
        ("home-vanguard", |s| {
            with_pack(s, "vanguard-drifter");
            home_busy(s);
        }),
        ("split2-akiba", |s| {
            with_pack(s, "akiba-overdrive");
            split2(s);
        }),
        ("splash-jade", |s| {
            with_pack(s, "jade-tempest");
            splash_scene(s);
        }),
        ("session", session_scene),
        ("split2", split2),
        ("split4", split4),
        ("settings", settings),
        ("tasks", tasks),
        ("inbox", inbox),
        ("diff", diff),
        ("viewer", viewer_code),
        ("viewer-markdown", viewer_markdown),
        ("viewer-image", viewer_image),
        ("viewer-video", viewer_video),
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
