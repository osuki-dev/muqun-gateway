// Session composer scenes (included into `scenes.rs`): a Claude Code session
// with a model that has effort levels, five permission modes (bypass on) and
// auto-approve, so the control row's collapse can be judged at any width
// (`--size`). Demo data only.

fn composer_catalog() -> std::sync::Arc<crate::hub::CatalogView> {
    let efforts = json!([{ "id": "low" }, { "id": "medium" }, { "id": "high" }, { "id": "max" }]);
    std::sync::Arc::new(crate::hub::CatalogView {
        catalog: serde_json::from_value(json!({
            "models": [
                { "id": "opus-5.5", "name": "Opus 5.5", "provider_id": "anthropic", "variants": efforts },
                { "id": "sonnet-5", "name": "Sonnet 5", "provider_id": "anthropic", "variants": efforts },
                { "id": "haiku-5", "name": "Haiku 5", "provider_id": "anthropic" },
            ],
            "modes": [
                { "id": "default", "name": "Default" },
                { "id": "acceptEdits", "name": "Accept edits" },
                { "id": "plan", "name": "Plan" },
                { "id": "dontAsk", "name": "Don't ask" },
                { "id": "bypassPermissions", "name": "Bypass permissions" },
            ],
            "mcp": [],
        }))
        .expect("demo catalog"),
    })
}

/// Make the focused session a Claude Code one on Opus 5.5 (medium effort),
/// bypassing permissions with auto-approve on, in a folder with two branches.
fn claude_session(s: &mut UiState) {
    s.agents.push(agent("claude", "Claude Code", true));
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == "s1") {
        info.agent_id = "claude".into();
        info.model = Some(muqun_gateway::agents::ModelRef {
            provider_id: "anthropic".into(),
            model_id: "opus-5.5".into(),
            variant: Some("medium".into()),
        });
        info.mode = Some("bypassPermissions".into());
    }
    s.catalog = Some(composer_catalog());
    s.auto_approve.insert("s1".into(), true);
    s.branches.insert(
        "/home/demo/gateway".into(),
        crate::model::compose::BranchInfo {
            current: Some("main".into()),
            local: vec!["main".into(), "desktop-polish".into()],
        },
    );
}

fn composer_session(s: &mut UiState, running: bool) {
    turn_state(s, running);
    claude_session(s);
}

fn composer_split(s: &mut UiState, running: bool) {
    split2(s);
    if let Some(info) = s.sessions.iter_mut().find(|i| i.asid.0 == "s1") {
        info.status = if running {
            muqun_gateway::agents::AgentSessionStatus::Busy
        } else {
            muqun_gateway::agents::AgentSessionStatus::Idle
        };
    }
    claude_session(s);
}

fn zh(s: &mut UiState) {
    s.cfg.general.language = crate::config::Language::ZhCn;
}

/// A prompt just sent to the Claude session, before the gateway's row for
/// it: `running` steers the turn, `picture` sends a pasted screenshot too.
fn echo_scene(s: &mut UiState, running: bool, words: &str, picture: bool) -> u64 {
    composer_session(s, running);
    let staged = if picture {
        vec![crate::model::compose::StagedFile {
            path: SCENE_PICTURE.into(),
            name: "screenshot.png".into(),
            size: 1,
            image: true,
        }]
    } else {
        Vec::new()
    };
    let token = s.token(super::Slot::Session);
    s.pending_submit = None;
    s.echo_sent("s1", token, words.into(), &staged, running);
    token
}

fn composer_scenes() -> Vec<Scene> {
    vec![
        // Sent while the agent works: on its way.
        ("echo-sending", |s| {
            echo_scene(s, true, "Also keep the badge hidden while the count loads.", true);
        }),
        // The gateway holds the steer for the agent's next step; a queued
        // prompt still waits in the strip.
        ("echo-steered", |s| {
            let token = echo_scene(s, true, "Also keep the badge hidden while the count loads.", false);
            s.echo_submitted(token, true);
            let inbox = vec![
                InboxItem {
                    id: "q1".into(),
                    text: "Also keep the badge hidden while the count loads.".into(),
                    steer: true,
                    files: vec![],
                },
                InboxItem {
                    id: "q2".into(),
                    text: "Then update the changelog.".into(),
                    steer: false,
                    files: vec![],
                },
            ];
            s.echo_inbox("s1", &inbox);
            s.queue = inbox;
        }),
        ("echo-failed", |s| {
            let token = echo_scene(s, false, "Run the full test suite again.", false);
            s.echo_submitted(token, false);
        }),
        ("echo-failed-zh", |s| {
            let token = echo_scene(s, false, "Run the full test suite again.", false);
            s.echo_submitted(token, false);
            zh(s);
        }),
        ("composer", |s| composer_session(s, true)),
        ("composer-idle", |s| composer_session(s, false)),
        ("composer-zh", |s| {
            composer_session(s, true);
            zh(s);
        }),
        ("composer-idle-zh", |s| {
            composer_session(s, false);
            zh(s);
        }),
        ("composer-split", |s| composer_split(s, true)),
        ("composer-split-idle", |s| composer_split(s, false)),
        ("composer-split-zh", |s| {
            composer_split(s, true);
            zh(s);
        }),
        ("composer-menu-more", |s| {
            composer_split(s, true);
            s.menu = Some(super::OpenMenu::ComposerMore);
        }),
        ("composer-menu-model", |s| {
            composer_split(s, true);
            s.menu = Some(super::OpenMenu::Model);
        }),
        ("composer-menu-mode", |s| {
            composer_split(s, true);
            s.menu = Some(super::OpenMenu::Mode);
        }),
    ]
}
