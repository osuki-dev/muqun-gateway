//! Settings → General: language, where new sessions start, notifications.

use std::path::PathBuf;

use rune::{El, TextEdit, Tx};
use rune_kit::{Kit, TextInput};

use super::{choice, edit_of, field, flag, group, row, select};
use crate::config::{ConfigEdit, Language};
use crate::platform::editors::Editor;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

pub(super) fn page(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    let language = s.cfg.general.language;
    let lang_at = match language {
        Language::En => 0,
        Language::ZhCn => 1,
        Language::ZhTw => 2,
    };
    let editor = edit_of(s, "editor", || {
        s.cfg.general.editor_command.clone().unwrap_or_default()
    });
    let notify = s.cfg.notifications.enabled;
    let tray = s.cfg.general.close_to_tray;
    let notify_title = tr_ref(language, "System notifications", "系统通知");
    let tray_title = tr_ref(language, "Keep running in the tray", "关闭窗口后留在托盘");
    vec![
        group(
            k,
            "general-language",
            tr_ref(language, "Language", "语言"),
            vec![row(
                k,
                "language",
                tr_ref(language, "Interface language", "界面语言"),
                tr_ref(
                    language,
                    "Applies at once to everything in this window.",
                    "立即作用于窗口中的所有文字。",
                ),
                choice(
                    k,
                    "language",
                    &["English", "简体中文", "繁體中文"],
                    lang_at,
                    |s, i| {
                        s.edit(ConfigEdit::Language(
                            [Language::En, Language::ZhCn, Language::ZhTw][i],
                        ));
                    },
                ),
            )],
        ),
        group(
            k,
            "general-workspace",
            tr_ref(language, "New sessions", "新会话"),
            vec![
                row(
                    k,
                    "cwd",
                    tr_ref(language, "Default folder", "默认文件夹"),
                    tr_ref(
                        language,
                        "Where a new session starts when its agent remembers no folder.",
                        "Agent 没有记住文件夹时，新会话从这里开始。",
                    ),
                    field(
                        TextInput::new(&s.cwd_input)
                            .label(tr_ref(language, "Default folder", "默认文件夹"))
                            .placeholder(tr_ref(language, "Home folder", "用户主目录"))
                            .on_edit(|s: &mut UiState| &mut s.cwd_input)
                            .on_change(|s: &mut UiState, _: &mut Tx| {
                                let text = s.cwd_input.text.trim().to_string();
                                s.edit(ConfigEdit::DefaultCwd(
                                    (!text.is_empty()).then(|| PathBuf::from(text)),
                                ));
                            }),
                        k,
                        "cwd",
                    ),
                ),
                row(
                    k,
                    "open-in",
                    tr_ref(language, "Open projects in", "打开项目的应用"),
                    tr_ref(
                        language,
                        "What “Open in editor” opens a project folder with.",
                        "「在编辑器中打开」使用的应用。",
                    ),
                    select(
                        s,
                        k,
                        layer,
                        "open-in",
                        &["VS Code", "Zed", "Cursor", "Terminal", "Files"],
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
                    tr_ref(language, "Editor command", "编辑器命令"),
                    tr_ref(
                        language,
                        "Optional: a command to run instead, given the folder.",
                        "可选：改用这条命令打开文件夹。",
                    ),
                    field(
                        TextInput::new(&editor)
                            .label(tr_ref(language, "Editor command", "编辑器命令"))
                            .placeholder("code, zed, …")
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
            tr_ref(language, "Notifications", "通知"),
            vec![
                row(
                    k,
                    "notify",
                    notify_title,
                    tr_ref(
                        language,
                        "When a session you are not looking at needs approval, asks a question or finishes.",
                        "不在眼前的会话需要审批、提问或完成时提醒你。",
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
                        "Closing the window leaves Muqun in the tray, still notifying. Quit from the tray's menu.",
                        "关闭窗口后 Muqun 留在托盘并继续提醒；从托盘菜单退出。",
                    ),
                    flag(k, "tray", tray, tray_title, move |s| {
                        s.edit(ConfigEdit::CloseToTray(!tray))
                    }),
                ),
            ],
        ),
    ]
}
