//! Settings → General: language, where new sessions start, notifications.

use std::path::PathBuf;

use rune::{El, TextEdit, Tx};
use rune_kit::{Kit, TextInput};

use super::{edit_of, field, flag, group, row, select};
use crate::config::{ConfigEdit, Language};
use crate::platform::editors::Editor;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

pub(super) fn page(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    let language = s.cfg.general.language;
    let lang_at = Language::ALL
        .iter()
        .position(|&l| l == language)
        .unwrap_or(0);
    let editor = edit_of(s, "editor", || {
        s.cfg.general.editor_command.clone().unwrap_or_default()
    });
    let notify = s.cfg.notifications.enabled;
    let tray = s.cfg.general.close_to_tray;
    let notify_title = tr_ref(language, "System notifications");
    let tray_title = tr_ref(language, "Keep running in the tray");
    vec![
        group(
            k,
            "general-language",
            tr_ref(language, "Language"),
            vec![row(
                k,
                "language",
                tr_ref(language, "Interface language"),
                tr_ref(
                    language,
                    "Applies at once to everything in this window."
                ),
                select(
                    s,
                    k,
                    layer,
                    "language",
                    Language::LABELS,
                    lang_at,
                    |s, i| {
                        s.edit(ConfigEdit::Language(Language::ALL[i]));
                    },
                ),
            )],
        ),
        group(
            k,
            "general-workspace",
            tr_ref(language, "New sessions"),
            vec![
                row(
                    k,
                    "cwd",
                    tr_ref(language, "Default folder"),
                    tr_ref(
                        language,
                        "Where a new session starts when its agent remembers no folder."
                    ),
                    field(
                        TextInput::new(&s.cwd_input)
                            .label(tr_ref(language, "Default folder"))
                            .placeholder(tr_ref(language, "Home folder"))
                            .on_edit(|s: &mut UiState| &mut s.cwd_input)
                            .on_change(|s: &mut UiState, _: &mut Tx| {
                                let text = s.cwd_input.text.trim().to_string();
                                s.edit(ConfigEdit::DefaultCwd(
                                    (!text.is_empty()).then(|| PathBuf::from(text)),
                                ));
                            }),
                        &s.cwd_input,
                        k,
                        "cwd",
                    ),
                ),
                row(
                    k,
                    "open-in",
                    tr_ref(language, "Open projects in"),
                    tr_ref(
                        language,
                        "What “Open in editor” opens a project folder with."
                    ),
                    select(
                        s,
                        k,
                        layer,
                        "open-in",
                        &Editor::LABELS,
                        Editor::ALL
                            .iter()
                            .position(|e| *e == s.cfg.general.editor)
                            .unwrap_or(0),
                        |s, i| s.edit(ConfigEdit::Editor(Editor::ALL[i])),
                    ),
                ),
                row(
                    k,
                    "editor",
                    tr_ref(language, "Editor command"),
                    tr_ref(
                        language,
                        "Optional: a command to run instead, given the folder."
                    ),
                    field(
                        TextInput::new(&editor)
                            .label(tr_ref(language, "Editor command"))
                            .placeholder("code, zed, nvim, …")
                            .on_edit(|s: &mut UiState| {
                                let stored =
                                    s.cfg.general.editor_command.clone().unwrap_or_default();
                                s.agent_inputs
                                    .entry("editor".into())
                                    .or_insert_with(|| TextEdit::new(stored))
                            })
                            .on_change(|s: &mut UiState, _: &mut Tx| {
                                let text = s
                                    .agent_inputs
                                    .get("editor")
                                    .map(|e| e.text.clone())
                                    .unwrap_or_default();
                                s.edit(ConfigEdit::EditorCommand(
                                    (!text.trim().is_empty()).then_some(text),
                                ));
                            }),
                        &editor,
                        k,
                        "editor",
                    ),
                ),
            ],
        ),
        // The App's "Gateway notifications": an approval, a question or a
        // finished turn in a session that is not on screen.
        group(
            k,
            "general-notify",
            tr_ref(language, "Notifications"),
            vec![
                row(
                    k,
                    "notify",
                    notify_title,
                    tr_ref(
                        language,
                        "When a session you are not looking at needs approval, asks a question or finishes."
                    ),
                    flag(k, "notify", notify, notify_title, move |s| {
                        s.edit(ConfigEdit::Notifications(!notify))
                    }),
                ),
                row(
                    k,
                    "tray",
                    tray_title,
                    tr_ref(
                        language,
                        "Closing the window leaves Muqun in the tray, still notifying. Quit from the tray's menu."
                    ),
                    flag(k, "tray", tray, tray_title, move |s| {
                        s.edit(ConfigEdit::CloseToTray(!tray))
                    }),
                ),
            ],
        ),
    ]
}
