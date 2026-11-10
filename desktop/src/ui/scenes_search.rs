// Sidebar search scenes (included into `scenes.rs`): message hits under the
// title matches, CJK highlights, the index still building, nothing found, a
// row lit after opening a hit, and the gateway link down. Demo data only.

/// A hit whose highlights are every occurrence of `terms` in `snippet`
/// (case-insensitive for ASCII), in UTF-16 units as the gateway sends them.
#[allow(clippy::too_many_arguments)]
fn search_hit(
    asid: &str,
    agent: &str,
    title: &str,
    item: &str,
    role: &str,
    kind: &str,
    snippet: &str,
    terms: &[&str],
    session_hits: usize,
    age: u64,
) -> crate::hub::SearchHit {
    let lower = snippet.to_lowercase();
    let mut highlights = Vec::new();
    for term in terms {
        let term = term.to_lowercase();
        for (at, _) in lower.match_indices(&term) {
            let start = lower[..at].encode_utf16().count();
            highlights.push([start, start + term.encode_utf16().count()]);
        }
    }
    highlights.sort();
    crate::hub::SearchHit {
        asid: asid.into(),
        agent_id: agent.into(),
        title: title.into(),
        item_id: item.into(),
        message_id: format!("m-{item}"),
        role: role.into(),
        kind: kind.into(),
        time_ms: super::now_ms().saturating_sub(age),
        snippet: snippet.into(),
        highlights,
        session_hits,
        ..Default::default()
    }
}

fn search_scene(s: &mut UiState, query: &str, hits: Vec<crate::hub::SearchHit>) {
    latest_scene(s);
    s.search = TextEdit::new(query);
    s.search.focused = true;
    s.draft.focused = false;
    s.content.supported = true;
    s.content.query = query.into();
    s.content.hits = hits;
}

fn search_en(s: &mut UiState) {
    let t = ["greet"];
    search_scene(
        s,
        "greet",
        vec![
            search_hit("s1", "codex", "Fix the greeting", "u1", "user", "text",
                "Make `greet` trim its argument and add a trailing `!`.", &t, 5, 60_000),
            search_hit("s1", "codex", "Fix the greeting", "a1", "assistant", "text",
                "I'll update greet and run the tests.", &t, 5, 50_000),
            search_hit("s1", "codex", "Fix the greeting", "t1", "assistant", "tool",
                "cargo test greet -- --nocapture", &t, 5, 40_000),
            search_hit("s3", "codex", "Port the settings page", "x1", "assistant", "text",
                "…the welcome card calls greet() before the settings load, so the name is empty on first run…", &t, 1, 3 * 3_600_000),
            search_hit("s2", "opencode", "Review the release notes", "x2", "user", "text",
                "Mention the new greeting in the notes.", &t, 2, 86_400_000),
        ],
    );
    s.content.next_cursor = Some("20".into());
}

fn search_zh(s: &mut UiState) {
    let t = ["database", "migration"];
    search_scene(
        s,
        "database migration",
        vec![
            search_hit("s4", "opencode", "Migration config loader", "z1", "user", "text",
                "Migrate configuration from JSON to database, keeping a backup for rollback.", &t, 4, 120_000),
            search_hit("s4", "opencode", "Migration config loader", "z2", "assistant", "text",
                "Completed database migration script: added settings table, auto-migrating legacy config.json on startup.", &t, 4, 100_000),
            search_hit("s2", "opencode", "Review the release notes", "z3", "assistant", "text",
                "Added release note: database migration runs on first startup and takes a few seconds.", &t, 1, 2 * 86_400_000),
        ],
    );
    s.sessions.push(session("s4", "Migration config loader", "opencode", "idle", "/home/demo/rune", 120_000));
    s.cfg.general.language = crate::config::Language::ZhCn;
}

fn search_scenes() -> Vec<Scene> {
    vec![
        ("search", search_en),
        ("search-zh", search_zh),
        ("search-tw", |s| {
            search_zh(s);
            s.cfg.general.language = crate::config::Language::ZhTw;
        }),
        // ↓ moved onto the second hit.
        ("search-cursor", |s| {
            search_en(s);
            s.content.cursor = Some(super::search::Cursor::Hit(1));
        }),
        ("search-indexing", |s| {
            search_en(s);
            s.content.next_cursor = None;
            s.content.indexing = Some(crate::hub::SearchIndexing { done: 312, total: 340 });
        }),
        ("search-indexing-zh", |s| {
            search_zh(s);
            s.content.indexing = Some(crate::hub::SearchIndexing { done: 12, total: 340 });
        }),
        ("search-none", |s| {
            search_scene(s, "kubernetes", Vec::new());
        }),
        ("search-none-zh", |s| {
            search_scene(s, "deploy script", Vec::new());
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
        // Ctrl+Shift+F: message hits only.
        ("search-messages", |s| {
            search_en(s);
            s.content.only = true;
        }),
        // A hit opened: its row lit for a moment.
        ("search-opened", |s| {
            search_en(s);
            s.search.focused = false;
            if let Some(tl) = s.timelines.get_mut("s1") {
                // Lit from a moment in the future: stays lit however slow the render.
                tl.flash = Some((
                    "a1".into(),
                    std::time::Instant::now() + std::time::Duration::from_secs(60),
                ));
                tl.selected = Some("a1".into());
            }
        }),
        ("link-down", |s| {
            latest_scene(s);
            s.hub_label = "local gateway".into();
            s.hub_down = Some("Connection refused (os error 111)".into());
        }),
        ("link-down-zh", |s| {
            latest_scene(s);
            s.hub_label = "local gateway".into();
            s.hub_down = Some("Connection refused (os error 111)".into());
            s.cfg.general.language = crate::config::Language::ZhCn;
        }),
    ]
}
