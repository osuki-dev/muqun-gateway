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
    activate(s, &crate::theme::store::Loaded::Builtin(pack));
}

fn activate(s: &mut UiState, loaded: &crate::theme::store::Loaded) {
    let applied = crate::theme::service::applied_from(
        loaded,
        crate::config::Scheme::System,
        std::time::Instant::now(),
    );
    match applied.and_then(crate::theme::apply::activate) {
        Ok(_) => s.cfg.appearance.theme_id = loaded.manifest().id.clone(),
        Err(err) => tracing::error!("scene theme: {err:#}"),
    }
}

/// `MUQUN_SCENE_THEME` renders any scene in another pack: a built-in id, a
/// `.muqun-theme` package, or a directory holding `theme.json` (a source in
/// the themes repository) or `manifest.json` (an installed theme) beside
/// `assets/`. Pair it with Rune's `--theme light|dark`.
fn scene_pack(s: &mut UiState) {
    let Ok(spec) = std::env::var("MUQUN_SCENE_THEME") else {
        return;
    };
    let path = std::path::Path::new(&spec);
    let loaded = if let Some(pack) = crate::theme::store::builtin(&spec) {
        Ok(crate::theme::store::Loaded::Builtin(pack))
    } else if path.is_dir() {
        ["theme.json", "manifest.json"]
            .iter()
            .map(|name| path.join(name))
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow::anyhow!("{spec}: no theme.json or manifest.json"))
            .and_then(|p| Ok(std::fs::read(p)?))
            .and_then(|bytes| crate::theme::pack::parse_manifest(&bytes))
            .map(|manifest| crate::theme::store::Loaded::Installed {
                manifest: std::sync::Arc::new(manifest),
                dir: path.to_path_buf(),
            })
    } else {
        std::fs::read(path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| crate::theme::pack::unpack(&bytes))
            .map(|pack| crate::theme::store::Loaded::Builtin(std::sync::Arc::new(pack)))
    };
    match loaded {
        Ok(loaded) => activate(s, &loaded),
        Err(err) => tracing::error!("MUQUN_SCENE_THEME: {err:#}"),
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
    scene_pack(s);
    s.hub_label = "standalone".into();
    s.agents_seen = true;
    s.splash = None;
    s.cfg.general.welcomed = true;
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

/// Home on a well-used install: ten agents, a full recent grid, and the
/// sidebar's Running list open on sessions and tasks, one on another computer.
fn home_crowd(s: &mut UiState) {
    home_busy(s);
    s.agents = [
        ("claude", "Claude Code"),
        ("codex", "Codex"),
        ("opencode", "OpenCode"),
        ("cursor", "Cursor"),
        ("deepseek", "DeepSeek"),
        ("qoder", "Qoder"),
        ("pi", "Pi"),
        ("t3", "T3 Code"),
        ("antigravity", "Antigravity"),
        ("fx", "FX"),
    ]
    .iter()
    .map(|(id, name)| agent(id, name, true))
    .collect();
    for (asid, title, agent_id, status, dir, age) in [
        (
            "s5",
            "Rework the theme loader",
            "claude",
            "busy",
            "/home/demo/gateway",
            9_000,
        ),
        (
            "s6",
            "Tidy the release notes",
            "claude",
            "idle",
            "/home/demo/rune",
            7_200_000,
        ),
        (
            "s7",
            "Explain the sync protocol",
            "deepseek",
            "idle",
            "/home/demo/gateway",
            86_400_000,
        ),
        (
            "s8",
            "Speed up the session list",
            "cursor",
            "idle",
            "/home/demo/rune",
            90_000_000,
        ),
        (
            "s9",
            "Draft the install guide",
            "claude",
            "idle",
            "/home/demo/site",
            200_000_000,
        ),
    ] {
        s.sessions
            .push(session(asid, title, agent_id, status, dir, age));
    }
    let now = super::now_ms();
    s.busy_since.insert("s4".into(), now - 4 * 60_000);
    s.busy_since.insert("s5".into(), now - 38 * 60_000);
    s.tasks = vec![
        TaskRow {
            id: "k1".into(),
            state: "working".into(),
            title: "Audit the release workflow".into(),
            progress: Some("Reading .github/workflows/release.yml".into()),
            agent_id: Some("codex".into()),
            asid: Some("s1".into()),
            started_ms: now - 12 * 60_000,
            ..TaskRow::default()
        },
        TaskRow {
            id: "k2".into(),
            state: "working".into(),
            title: "Run the device test matrix".into(),
            progress: Some("iOS 3 of 8".into()),
            agent_id: Some("claude".into()),
            machine: Some("studio".into()),
            started_ms: now - 71 * 60_000,
            ..TaskRow::default()
        },
        TaskRow {
            id: "k3".into(),
            state: "working".into(),
            kind: "command".into(),
            title: "cargo build --release".into(),
            started_ms: now - 2 * 60_000,
            ..TaskRow::default()
        },
    ];
    s.cfg.ui.running_open = true;
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
                draft: TextEdit::new("Also check the Chinese copy"),
                ..SessionSlot::default()
            },
        },
    ];
    s.timelines.insert(
        "s2".into(),
        {
            let mut tl = super::timeline::SessionTimeline::default();
            tl.replace(parked_items(
                "s2",
                    "Review the release notes for 3.1.0 and tighten the wording.",
                    "Reading `release-notes/unreleased.md`…\n\n- The **Pad layout** entry needs the keyboard shortcut.\n- Two entries repeat the PR number.",
            ));
            tl.earlier_exhausted = true;
            tl
        },
    );
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
                ..SessionSlot::default()
            },
        },
        Tile {
            asid: Some("s4".into()),
            slot: SessionSlot {
                ..SessionSlot::default()
            },
        },
        Tile {
            asid: None,
            slot: SessionSlot::default(),
        },
    ];
    s.timelines.insert("s2".into(), {
        let mut tl = super::timeline::SessionTimeline::default();
        tl.replace(parked_items(
            "s2",
            "Review the release notes for 3.1.0.",
            "Two entries repeat the PR number; fixing.",
        ));
        tl.earlier_exhausted = true;
        tl
    });
    s.timelines.insert("s4".into(), {
        let mut tl = super::timeline::SessionTimeline::default();
        tl.replace(parked_items(
            "s4",
            "Move the config loader onto serde defaults.",
            "Working through `config.rs`: 3 of 7 structs done.",
        ));
        tl.earlier_exhausted = true;
        tl
    });
    s.focus = 1;
    // Tile 1 is focused: its state lives in the top-level fields.
    s.tiles[1].slot = SessionSlot::default();
    s.selected = Some("s2".into());
    s.draft = TextEdit::new("");
    s.demo = true;
}

fn settings(s: &mut UiState) {
    base(s);
    s.mode = Mode::Settings;
    s.settings_tab = SettingsTab::Agents;
    s.storage = Some(crate::model::storage::StorageReport {
        previews: 9_500_000,
        gallery: 88_000,
        uploads: 20_000_000,
        other: 1_200_000,
        state_dir: Some("/home/demo/.local/share/muqun-desktop".into()),
    });
    settings_themes(s);
    s.demo = true;
}

/// The built-in packs as the library (with thumbnails made from their
/// previews, as the theme service makes them) and again as the gallery.
fn settings_themes(s: &mut UiState) {
    use crate::theme::store::{builtin, Loaded, Origin, ThemeEntry, BUILTIN};
    let dir = std::env::temp_dir().join("muqun-scene-thumbs");
    let _ = std::fs::create_dir_all(&dir);
    let mut library = Vec::new();
    let mut gallery = Vec::new();
    for b in BUILTIN {
        let Some(pack) = builtin(b.id) else { continue };
        let loaded = Loaded::Builtin(pack.clone());
        let mut entry = ThemeEntry::from_manifest(&pack.manifest, Origin::Builtin);
        let path = dir.join(format!("{}.jpg", b.id));
        if !path.exists() {
            if let Some(jpeg) = loaded
                .preview()
                .and_then(|p| crate::theme::gallery::thumbnail(&p).ok())
            {
                let _ = std::fs::write(&path, jpeg);
            }
        }
        if path.exists() {
            entry.thumbnail = Some(path.clone());
            s.theme_previews.insert(b.id.to_string(), path);
        }
        gallery.push(crate::theme::gallery::IndexEntry {
            id: entry.id.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            author: entry.author.clone(),
            license: entry.license.clone(),
            description: entry.description.clone(),
            tags: entry.tags.clone(),
            package: format!("dist/{}.muqun-theme", b.id),
            bytes: b.bytes.len() as u64,
            sha256: None,
            assets: None,
            preview: None,
        });
        library.push(entry);
    }
    // The gallery also lists themes not installed here.
    library.truncate(4);
    s.theme_library = std::sync::Arc::new(library);
    s.theme_gallery = std::sync::Arc::new(crate::theme::service::GalleryView {
        entries: gallery,
        ..Default::default()
    });
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
include!("scenes_terminal.rs");
include!("scenes_search.rs");
include!("scenes_composer.rs");
include!("scenes_lists.rs");
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
        docked: false,
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

fn voice_meter() -> std::sync::Arc<crate::model::voice::Meter> {
    use crate::model::voice::Meter;
    let started = std::time::Instant::now() - std::time::Duration::from_secs_f64(6.3);
    let meter = Meter::since(started);
    // A speech-like envelope: syllables riding on phrases, a pause mid-way.
    let mut t = 0.0f64;
    while t < 6.3 {
        let phrase =
            ((t * 0.9).sin() * 0.5 + 0.5) * if (2.6..3.2).contains(&t) { 0.1 } else { 1.0 };
        let syllable = ((t * 11.0).sin() * 0.5 + 0.5).powf(0.7);
        meter.push(t, (0.12 + 0.78 * phrase * syllable) as f32);
        t += 0.02;
    }
    std::sync::Arc::new(meter)
}

fn voice_scene(s: &mut UiState) {
    use crate::config::VoiceMode;
    session_scene(s);
    s.cfg.voice.mode = VoiceMode::File;
    s.voice = Some(super::voice::Dictation::preview(
        VoiceMode::File,
        voice_meter(),
        "",
    ));
}

fn voice_realtime_scene(s: &mut UiState) {
    use crate::config::VoiceMode;
    session_scene(s);
    s.cfg.voice.mode = VoiceMode::Realtime;
    s.voice = Some(super::voice::Dictation::preview(
        VoiceMode::Realtime,
        voice_meter(),
        "Rename the greet helper and 顺便把测试也更新一下, then run the checks",
    ));
}

fn voice_settings_scene(s: &mut UiState) {
    use crate::config::VoiceMode;
    settings(s);
    s.settings_tab = SettingsTab::Voice;
    s.cfg.voice.mode = VoiceMode::Realtime;
    s.cfg.voice.url = "wss://speech.example.com/v1/realtime?intent=transcription&model=live".into();
    s.voice_form = super::voice::VoiceForm::new(&s.cfg.voice);
    s.voice_form.key_saved = Some(crate::platform::secrets::Store::Keyring);
}

fn palette_scene(s: &mut UiState) {
    session_scene(s);
    s.cfg.ui.recent_commands = vec!["go:Tasks".into(), "session:s2".into()];
    s.palette = Some(super::palette::Palette::new(false));
}

fn palette_search_scene(s: &mut UiState) {
    session_scene(s);
    let mut palette = super::palette::Palette::new(false);
    palette.query = TextEdit::new("new cod");
    palette.query.focused = true;
    palette.selected = 1;
    s.cfg.general.last_cwd.clear();
    s.cfg
        .general
        .last_cwd
        .insert("codex".into(), "/home/demo/gateway".into());
    s.cfg
        .general
        .last_cwd
        .insert("opencode".into(), "/home/demo/rune".into());
    s.palette = Some(palette);
}

fn shortcuts_scene(s: &mut UiState) {
    session_scene(s);
    s.toggle_keys_sheet();
}

fn latest_scene(s: &mut UiState) {
    session_scene(s);
    s.right_open = false;
    s.right.set_open(false);
}

/// A scene name and the state it applies.
pub type Scene = (&'static str, fn(&mut UiState));

pub fn scenes() -> &'static [Scene] {
    static ALL: std::sync::OnceLock<Vec<Scene>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut all = base_scenes().to_vec();
        all.extend(search_scenes());
        all.extend(composer_scenes());
        all.extend(list_scenes());
        all
    })
}

fn base_scenes() -> &'static [Scene] {
    &[
        ("home", home),
        ("home-busy", home_busy),
        ("home-crowd", home_crowd),
        ("home-folders", |s| {
            home_crowd(s);
            for (dir, branch) in [
                ("/home/demo/gateway", "main"),
                ("/home/demo/rune", "desktop-polish"),
            ] {
                s.directories.insert(
                    dir.into(),
                    crate::model::projects::DirInfo {
                        root: dir.into(),
                        kind: crate::model::projects::ProjectKind::Git,
                        branch: Some(branch.into()),
                        exists: true,
                    },
                );
            }
            s.menu = Some(super::OpenMenu::StartFolder);
        }),
        ("home-crowd-zh", |s| {
            home_crowd(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
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
        // Split view with the focused composer's field in focus.
        ("split2-typing", |s| {
            split2(s);
            s.draft = TextEdit::new("Run the tests again");
            s.draft.focused = true;
        }),
        // Four panes (one tall, one above two) on an opaque window.
        ("split4-plain", |s| {
            split4(s);
            s.cfg.appearance.reduced_motion = true;
            s.cfg.appearance.reduced_transparency = true;
        }),
        // Split view on an opaque window without motion, sidebar closed.
        ("split2-plain", |s| {
            split2(s);
            s.cfg.appearance.reduced_motion = true;
            s.cfg.appearance.reduced_transparency = true;
            s.sidebar.set_open(false);
        }),
        ("split2-zh", |s| {
            split2(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("home-noagent", |s| {
            home_first(s);
            for a in &mut s.agents {
                a.enabled = false;
            }
        }),
        ("welcome", |s| {
            home_first(s);
            s.cfg.general.welcomed = false;
        }),
        ("settings-appearance", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Appearance;
        }),
        ("settings-general", |s| {
            settings(s);
            s.settings_tab = SettingsTab::General;
        }),
        ("settings-shortcuts", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Shortcuts;
        }),
        ("settings-about", |s| {
            settings(s);
            s.settings_tab = SettingsTab::About;
        }),
        ("settings-themes", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Themes;
            s.theme_tab = super::settings::ThemeTab::Installed;
        }),
        ("settings-themes-gallery", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Themes;
            s.theme_tab = super::settings::ThemeTab::Gallery;
        }),
        ("settings-themes-search", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Themes;
            s.theme_query = TextEdit::new("jade");
        }),
        ("settings-storage", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Storage;
        }),
        ("settings-open-in", |s| {
            settings(s);
            s.settings_tab = SettingsTab::General;
            s.menu = Some(super::OpenMenu::Setting("open-in".into()));
        }),
        ("settings-language", |s| {
            settings(s);
            s.settings_tab = SettingsTab::General;
            s.menu = Some(super::OpenMenu::Setting("language".into()));
        }),
        ("settings-agents-zh", |s| {
            settings(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-appearance-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Appearance;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-general-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::General;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-themes-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Themes;
            s.theme_tab = super::settings::ThemeTab::Installed;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-themes-gallery-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Themes;
            s.theme_tab = super::settings::ThemeTab::Gallery;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-shortcuts-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Shortcuts;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-storage-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::Storage;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("settings-about-zh", |s| {
            settings(s);
            s.settings_tab = SettingsTab::About;
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("voice-settings-zh", |s| {
            voice_settings_scene(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("home-big", |s| {
            home_busy(s);
            s.cfg.appearance.text_scale = 1.2;
            s.cfg.appearance.shape = crate::config::Shape::Round;
        }),
        ("home-tw", |s| {
            home_busy(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("session-tw", |s| {
            session_scene(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("settings-tw", |s| {
            settings(s);
            s.settings_tab = SettingsTab::General;
            s.cfg.general.language = crate::config::Language::ZhTw;
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
        ("settings-themes-akiba", |s| {
            with_pack(s, "akiba-overdrive");
            settings(s);
            s.settings_tab = SettingsTab::Themes;
        }),
        ("splash-jade", |s| {
            with_pack(s, "jade-tempest");
            splash_scene(s);
        }),
        ("session", session_scene),
        ("split2", split2),
        ("split4", split4),
        // Three panes: one on the left beside two stacked.
        ("split3", |s| {
            split4(s);
            s.tiles.pop();
        }),
        // Two panes with the terminal open on the right.
        ("split2-terminal", |s| {
            split2(s);
            s.term.fixture = Some(shell_fixture);
            s.term.open = true;
            s.term.pane.set_open(true);
        }),
        ("settings", settings),
        ("tasks", tasks),
        ("inbox", inbox),
        ("diff", diff),
        ("diff-card", |s| {
            session_scene(s);
            s.inspect = Some(super::Inspect {
                asid: "s1".into(),
                id: "t1".into(),
            });
            s.inspect_pane.set_open(true);
            s.inspect_pane.set_width(550.0);
        }),
        ("timeline-cards", timeline_cards),
        ("timeline-narrow", timeline_narrow),
        ("timeline-long", timeline_long),
        ("timeline-zh", timeline_zh),
        ("timeline-grouped", timeline_grouped),
        ("timeline-expanded", timeline_expanded),
        ("timeline-running", timeline_running),
        ("timeline-detail", timeline_detail),
        // A message's hover actions: hover it with `--script`, a
        // `{"type": "move", "key": "item-a1"}` step (the owner's message is
        // `"key": "user"`, on screen at `--size 1440x1600`).
        ("message-actions", |s| {
            session_scene(s);
            s.right_open = false;
            s.right.set_open(false);
        }),
        ("session-narrow", |s| {
            // A wide right-hand pane squeezes the session column to ~360 pt
            // at 1440 (and ~240 pt at 1320).
            timeline_detail(s);
            s.sessions[0].status = muqun_gateway::agents::AgentSessionStatus::Busy;
            s.inspect_pane.set_width(700.0);
            s.diff_stat.files = 3;
            s.diff_stat.added = 3;
            s.diff_stat.removed = 1;
        }),
        ("popup-home-mention", popup_home_mention),
        ("popup-home-slash", popup_home_slash),
        ("popup-session-slash", popup_session_slash),
        ("popup-session-skill", popup_session_skill),
        ("timeline-detail-zh", |s| {
            timeline_detail(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("terminal", |s| terminal_scene(s, shell_fixture)),
        ("terminal-tabs", |s| {
            terminal_scene(s, shell_fixture);
            s.term.tabs = vec![
                super::terminal_panel::TermTab {
                    id: 1,
                    session: None,
                    frame: None,
                    cwd: std::path::PathBuf::from("/home/ryu/Work/muqun/gateway-desktop"),
                    title: Some("muqun/gateway-desktop".into()),
                    pending_command: None,
                },
                super::terminal_panel::TermTab {
                    id: 2,
                    session: None,
                    frame: None,
                    cwd: std::path::PathBuf::from("/home/ryu/Work/muqun/server"),
                    title: Some("nvim src/main.rs".into()),
                    pending_command: None,
                },
            ];
            s.term.active_tab = 0;
        }),
        ("terminal-vim", |s| terminal_scene(s, vim_fixture)),
        ("terminal-ended", |s| {
            terminal_scene(s, shell_fixture);
            s.term.fixture_ended = Some(crate::term::session::Ended::Shell(
                crate::platform::pty::Exit::Code(3),
            ));
        }),
        ("terminal-plain", |s| {
            terminal_scene(s, shell_fixture);
            s.cfg.appearance.reduced_transparency = true;
        }),
        ("timeline-detail-plain", |s| {
            timeline_detail(s);
            s.cfg.appearance.reduced_transparency = true;
        }),
        ("terminal-menu", |s| {
            terminal_scene(s, shell_fixture);
            s.term.menu_at = Some(rune::PointerPress {
                x: 1180.0,
                y: 300.0,
            });
        }),
        ("terminal-ime", |s| {
            terminal_scene(s, shell_fixture);
            s.term
                .sink
                .ime(&rune::Ime::Preedit("ni hao".into(), Some((6, 6))));
        }),
        ("terminal-jade", |s| {
            with_pack(s, "jade-tempest");
            terminal_scene(s, shell_fixture);
        }),
        ("voice-settings-tw", |s| {
            voice_settings_scene(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("voice-tw", |s| {
            voice_scene(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("palette-tw", |s| {
            palette_scene(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("viewer-tw", |s| {
            viewer_code(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("viewer-video-tw", |s| {
            viewer_video(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        ("voice", voice_scene),
        ("voice-realtime", voice_realtime_scene),
        ("voice-settings", voice_settings_scene),
        ("timeline-subagent", timeline_subagent),
        ("timeline-context", timeline_context),
        ("palette", palette_scene),
        ("quick-open", |s| {
            session_scene(s);
            s.palette = Some(super::palette::Palette::new(true));
        }),
        // The shortcuts sheet (Ctrl+/).
        ("shortcuts", shortcuts_scene),
        ("shortcuts-zh", |s| {
            shortcuts_scene(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("shortcuts-search", |s| {
            shortcuts_scene(s);
            if let Some(sheet) = s.keys_sheet.as_mut() {
                sheet.query = TextEdit::new("split");
                sheet.query.focused = true;
            }
        }),
        // The "Latest" pill shows once the reader scrolls up: run with
        // `--script` and a `{"type": "wheel", "key": "feed-s1", "dy": 400}`
        // step. `latest-new` has three messages waiting below.
        ("latest", latest_scene),
        ("latest-new", |s| {
            latest_scene(s);
            if let Some(tl) = s.timelines.get_mut("s1") {
                tl.unseen = 3;
            }
        }),
        ("palette-search", palette_search_scene),
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
