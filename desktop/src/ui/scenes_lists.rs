// Inbox and Tasks scenes (included into `scenes.rs`): a busy inbox across
// sessions, a dangerous command, nothing waiting; a task table across
// computers with a history, filtered, empty and unreachable. Demo data only.

fn waiting_since(s: &mut UiState, key: &str, age: u64) {
    s.inbox_view
        .since
        .insert(key.into(), super::now_ms().saturating_sub(age));
}

fn perm_json(value: serde_json::Value) -> PermissionRequest {
    serde_json::from_value(value).expect("demo permission")
}

/// Three sessions waiting: a command and a question, an edit with its diff,
/// a plan to review, and a paused task on another computer.
fn inbox_busy(s: &mut UiState) {
    base(s);
    // The open session's timeline: what the agent said before it asked.
    timeline(s);
    if let Some(tl) = s.timelines.get_mut("s1") {
        tl.items.push(item(
            "a2",
            TimelineRole::Assistant,
            AgentPart::Text {
                text: "The greeting now trims its input. I'd like to run the whole test \
                       suite before I commit, since `greet` is used by the CLI too."
                    .into(),
            },
        ));
    }
    s.sessions.push(session(
        "s4",
        "Rename the config loader and update every caller",
        "opencode",
        "idle",
        "/home/demo/rune",
        5_000,
    ));
    s.waiting_perms.insert("s1".into(), vec![permission()]);
    s.waiting_forms.insert("s1".into(), vec![form()]);
    s.waiting_perms.insert(
        "s4".into(),
        vec![perm_json(json!({
            "id": "perm-2", "asid": "s4", "action": "edit", "resources": ["src/lib.rs"],
            "save": ["src/**"], "prompt": "Edit src/lib.rs?", "options": [],
            "metadata": { "files": [{ "file": "src/lib.rs", "patch": PATCH }] },
        }))],
    );
    s.waiting_perms.insert(
        "s3".into(),
        vec![perm_json(json!({
            "id": "perm-3", "asid": "s3", "action": "plan", "tool": "ExitPlanMode",
            "resources": [], "prompt": "Ready to code?",
            "message": "1. Move the settings rows into one grid\n2. Fold the nav into a rail under 820 pt\n3. Keep every control at toolbar height",
            "options": [],
        }))],
    );
    s.tasks = vec![TaskRow {
        id: "k9".into(),
        state: "waiting".into(),
        title: "Publish the preview build".into(),
        goal: "Publish the preview build once CI is green".into(),
        progress: Some("Waiting for you to confirm the release notes".into()),
        origin: "device".into(),
        completable: true,
        machine: Some("studio".into()),
        agent_id: Some("claude".into()),
        updated_ms: super::now_ms() - 26 * 60_000,
        ..TaskRow::default()
    }];
    waiting_since(s, "p:perm-1", 3 * 60_000);
    waiting_since(s, "f:form-1", 2 * 60_000);
    waiting_since(s, "p:perm-2", 9 * 60_000);
    waiting_since(s, "p:perm-3", 40_000);
    s.inbox_view.pick = Some("p:perm-1".into());
    s.mode = Mode::Inbox;
    s.demo = true;
}

fn inbox_danger(s: &mut UiState) {
    inbox_busy(s);
    s.waiting_perms.insert(
        "s1".into(),
        vec![perm_json(json!({
            "id": "perm-1", "asid": "s1", "action": "bash",
            "resources": ["rm -rf ~/"], "prompt": "Clean the workspace?", "options": [],
        }))],
    );
}

/// Tasks across this computer and two linked ones, in every state.
fn tasks_busy(s: &mut UiState) {
    base(s);
    let now = super::now_ms();
    let min = 60_000;
    let task = |id: &str, state: &str, title: &str| TaskRow {
        id: id.into(),
        state: state.into(),
        title: title.into(),
        goal: title.into(),
        origin: "device".into(),
        ..TaskRow::default()
    };
    s.tasks = vec![
        TaskRow {
            goal: "Audit the release workflow: check every job runs on the reviewed commit, \
                   that tags are never moved, and that notes are attached before binaries."
                .into(),
            progress: Some("Reading .github/workflows/release.yml".into()),
            agent_id: Some("codex".into()),
            asid: Some("s1".into()),
            started_ms: now - 12 * min,
            updated_ms: now - 20_000,
            children: crate::hub::TaskChildren {
                total: 2,
                active: 1,
                waiting: 0,
                failed: 0,
            },
            ..task("k1", "working", "Audit the release workflow")
        },
        TaskRow {
            parent_task_id: Some("k1".into()),
            progress: Some("iOS 3 of 8".into()),
            agent_id: Some("claude".into()),
            machine: Some("studio".into()),
            started_ms: now - 9 * min,
            updated_ms: now - min,
            ..task("k2", "working", "Run the device test matrix on the studio Mac")
        },
        TaskRow {
            parent_task_id: Some("k1".into()),
            summary: Some("No tag is moved; notes are checked before upload.".into()),
            agent_id: Some("codex".into()),
            started_ms: now - 11 * min,
            updated_ms: now - 6 * min,
            ..task("k3", "done", "Check the tag guard")
        },
        TaskRow {
            agent_id: Some("opencode".into()),
            asid: Some("s2".into()),
            completable: true,
            started_ms: now - 64 * min,
            updated_ms: now - 31 * min,
            ..task(
                "k4",
                "idle_unconfirmed",
                "Draft the migration notes for the config loader rename, with a before/after table",
            )
        },
        TaskRow {
            kind: "command".into(),
            exit_code: Some(101),
            summary: Some("2 tests failed in muqun-desktop".into()),
            started_ms: now - 3 * 60 * min,
            updated_ms: now - 3 * 60 * min + 4 * min,
            ..task("k5", "failed", "cargo test --workspace --locked")
        },
        TaskRow {
            agent_id: Some("claude".into()),
            machine: Some("build-box".into()),
            started_ms: now - 26 * 60 * min,
            updated_ms: now - 25 * 60 * min,
            ..task("k6", "cancelled", "Rebuild the Windows installer")
        },
        TaskRow {
            agent_id: Some("codex".into()),
            summary: Some("Bumped 14 crates; the lockfile builds clean.".into()),
            started_ms: now - 2 * 24 * 60 * min,
            updated_ms: now - 2 * 24 * 60 * min + 18 * min,
            ..task("k7", "done", "Bump dependencies")
        },
    ];
    s.task_cwd = Some("/home/demo/gateway".into());
    let step = |ago: u64, to: &str, note: Option<&str>| crate::hub::TaskStep {
        at_ms: now - ago,
        to: to.into(),
        note: note.map(str::to_string),
    };
    s.tasks_view.history.insert(
        "k1".into(),
        vec![
            step(12 * min, "queued", None),
            step(12 * min - 2_000, "dispatching", None),
            step(11 * min, "working", Some("Codex started in /home/demo/gateway")),
            step(7 * min, "waiting", Some("Asked to run the test suite")),
            step(6 * min, "working", None),
        ],
    );
    s.tasks_view.pick = Some("k1".into());
    s.mode = Mode::Tasks;
    s.demo = true;
}

fn list_scenes() -> Vec<Scene> {
    vec![
        ("inbox-busy", inbox_busy),
        ("inbox-danger", inbox_danger),
        ("inbox-edit", |s| {
            inbox_busy(s);
            s.inbox_view.pick = Some("p:perm-2".into());
        }),
        ("inbox-plan", |s| {
            inbox_busy(s);
            s.inbox_view.pick = Some("p:perm-3".into());
        }),
        ("inbox-task", |s| {
            inbox_busy(s);
            s.inbox_view.pick = Some("t:k9".into());
        }),
        ("inbox-empty", |s| {
            base(s);
            s.mode = Mode::Inbox;
            s.demo = true;
        }),
        ("inbox-zh", |s| {
            inbox_busy(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        ("tasks-busy", tasks_busy),
        ("tasks-paused", |s| {
            tasks_busy(s);
            s.tasks_view.filter = super::tasks::Filter::Paused;
            s.tasks_view.pick = Some("k4".into());
            s.tasks_view.arm_cancel = Some("k4".into());
        }),
        ("tasks-command", |s| {
            tasks_busy(s);
            s.tasks_view.pick = Some("k5".into());
        }),
        ("tasks-studio", |s| {
            tasks_busy(s);
            s.tasks_view.machine = Some(Some("studio".into()));
        }),
        ("tasks-empty", |s| {
            base(s);
            s.task_cwd = Some("/home/demo/gateway".into());
            s.mode = Mode::Tasks;
            s.demo = true;
        }),
        ("tasks-error", |s| {
            tasks_busy(s);
            s.tasks_view.error = Some("connection refused (127.0.0.1:23847)".into());
        }),
        ("tasks-error-empty", |s| {
            base(s);
            s.tasks_view.error = Some("timed out".into());
            s.mode = Mode::Tasks;
            s.demo = true;
        }),
        ("tasks-zh", |s| {
            tasks_busy(s);
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
    ]
}
