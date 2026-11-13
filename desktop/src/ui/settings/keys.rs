//! Settings → Shortcuts: every chord the window answers to (`ui/keymap.rs`,
//! the same rows the shortcuts sheet lists), with interactive customization
//! for the Global Leader Key and every action, plus live conflict checks.

use rune::{col, row as hrow, text, El, KeyPlatform};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::{button, group, group_action, row, status, Tone};
use crate::config::{ConfigEdit, KeymapConfig};
use crate::ui::copy::{tr, tr_ref};
use crate::ui::keymap::{self, Entry, Group, RecordingTarget};
use crate::ui::UiState;

const LEADER_PRESETS: [(&str, &str); 5] = [
    ("ctrl+space", "Ctrl+Space"),
    ("ctrl+;", "Ctrl+;"),
    ("alt+space", "Alt+Space"),
    ("ctrl+a", "Ctrl+A"),
    ("ctrl+g", "Ctrl+G"),
];

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    if s.cfg.keymap.is_default() {
        return Vec::new();
    }
    let language = s.cfg.general.language;
    vec![button(tr_ref(language, "Reset All to Defaults"))
        .on_press(|s: &mut UiState| {
            s.recording_key = None;
            s.edit(ConfigEdit::ResetKeymap);
        })
        .build(k)
        .key("sc-reset-all")]
}

fn custom_badge(k: &Kit, language: crate::config::Language, key: &str) -> El {
    hrow()
        .key(key.to_string())
        .h(20.0)
        .px(6.0)
        .center()
        .radius(k.radius.sm)
        .fill(k.c.bg_accent.alpha(0.16))
        .border(1.0, k.c.bg_accent.alpha(0.4))
        .child(
            text(tr_ref(language, "Custom"))
                .font(k.ty.small_medium)
                .color(k.c.text_accent),
        )
}

fn leader_group(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let leader_caps = keymap::leader_keycaps(KeyPlatform::current(), Some(&s.cfg.keymap));
    let is_custom_leader = s.cfg.keymap.leader != KeymapConfig::DEFAULT_LEADER;
    let recording_leader = s.recording_key == Some(RecordingTarget::Leader);

    let mut leader_controls = hrow().key("leader-controls").gap(8.0).center_y();
    leader_controls = leader_controls.child(crate::ui::shortcuts::keycaps(k, &leader_caps));
    if is_custom_leader {
        leader_controls = leader_controls.child(custom_badge(k, language, "sc-leader-custom"));
    }
    let record_btn = Button::new(if recording_leader {
        tr_ref(language, "Press keys…")
    } else {
        tr_ref(language, "Record Leader")
    })
    .variant(if recording_leader {
        ButtonVariant::Submit
    } else {
        ButtonVariant::Outline
    })
    .control_size(ControlSize::Small)
    .on_press(move |s: &mut UiState| {
        s.recording_key = if recording_leader {
            None
        } else {
            Some(RecordingTarget::Leader)
        };
    })
    .build(k)
    .key("sc-record-leader");
    leader_controls = leader_controls.child(record_btn);

    if is_custom_leader {
        let reset_btn = Button::new(tr_ref(language, "Reset"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(|s: &mut UiState| {
                s.recording_key = None;
                s.edit(ConfigEdit::SetLeader(
                    KeymapConfig::DEFAULT_LEADER.to_string(),
                ));
            })
            .build(k)
            .key("sc-reset-leader");
        leader_controls = leader_controls.child(reset_btn);
    }

    let mut preset_row = hrow().key("leader-presets").gap(6.0).center_y().flex_wrap();
    for (spec, label) in LEADER_PRESETS {
        let active = s.cfg.keymap.leader.eq_ignore_ascii_case(spec);
        let spec_owned = spec.to_string();
        preset_row = preset_row.child(
            Button::new(label)
                .variant(if active {
                    ButtonVariant::Contrast
                } else {
                    ButtonVariant::Outline
                })
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    s.recording_key = None;
                    s.edit(ConfigEdit::SetLeader(spec_owned.clone()));
                })
                .build(k)
                .key(format!("sc-leader-preset-{spec}")),
        );
    }

    let rows = vec![
        row(
            k,
            "leader-key",
            tr_ref(language, "Leader key"),
            tr_ref(
                language,
                "Press anywhere (including inside a focused terminal) to open the Which-Key overlay. Press twice in a terminal to send the key to the shell.",
            ),
            leader_controls,
        ),
        row(
            k,
            "leader-presets",
            tr_ref(language, "Quick presets"),
            tr_ref(
                language,
                "One-click leader chords for terminal and workspace navigation.",
            ),
            preset_row,
        ),
    ];

    if !s.cfg.keymap.is_default() {
        let reset_all = Button::new(tr_ref(language, "Reset All to Defaults"))
            .variant(ButtonVariant::Outline)
            .control_size(ControlSize::Small)
            .on_press(|s: &mut UiState| {
                s.recording_key = None;
                s.edit(ConfigEdit::ResetKeymap);
            })
            .build(k)
            .key("sc-group-reset-all");
        group_action(
            k,
            "shortcuts-leader",
            tr_ref(language, "Global Leader Key (Which-Key)"),
            reset_all,
            rows,
        )
    } else {
        group(
            k,
            "shortcuts-leader",
            tr_ref(language, "Global Leader Key (Which-Key)"),
            rows,
        )
    }
}

fn interactive_entry_row(s: &UiState, k: &Kit, entry: &'static Entry, key: String) -> El {
    let language = s.cfg.general.language;
    let action = entry.action;
    let action_id = action.id();
    let overridden = entry.is_overridden(&s.cfg.keymap);
    let recording = s.recording_key == Some(RecordingTarget::Action(action));

    let mut left = hrow()
        .key(format!("{key}-left"))
        .gap(8.0)
        .center_y()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .child(
            text(entry.what(language))
                .font(k.ty.ui)
                .color(k.c.text_base)
                .shrink()
                .min_w(0.0),
        );
    if overridden {
        left = left.child(custom_badge(k, language, &format!("{key}-custom")));
    }

    let mut right = hrow().key(format!("{key}-right")).gap(8.0).center_y();
    right = right.child(crate::ui::shortcuts::entry_keys(
        k,
        language,
        entry,
        Some(&s.cfg.keymap),
    ));

    let record_btn = Button::new(if recording {
        tr_ref(language, "Press keys…")
    } else {
        tr_ref(language, "Record")
    })
    .variant(if recording {
        ButtonVariant::Submit
    } else {
        ButtonVariant::Outline
    })
    .control_size(ControlSize::Small)
    .on_press(move |s: &mut UiState| {
        s.recording_key = if recording {
            None
        } else {
            Some(RecordingTarget::Action(action))
        };
    })
    .build(k)
    .key(format!("sc-record-{action_id}"));
    right = right.child(record_btn);

    if overridden {
        let reset_btn = Button::new(tr_ref(language, "Reset"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                if s.recording_key == Some(RecordingTarget::Action(action)) {
                    s.recording_key = None;
                }
                if action == keymap::Action::TermPrefix {
                    s.edit(ConfigEdit::SetLeader(
                        KeymapConfig::DEFAULT_LEADER.to_string(),
                    ));
                }
                s.edit(ConfigEdit::SetKeybind {
                    action: action_id.to_string(),
                    spec: None,
                });
            })
            .build(k)
            .key(format!("sc-reset-{action_id}"));
        right = right.child(reset_btn);
    }

    hrow()
        .key(key)
        .w_full()
        .gap(12.0)
        .center_y()
        .py(7.0)
        .child(left)
        .child(right)
}

pub(super) fn page(s: &UiState, k: &Kit) -> Vec<El> {
    keymap::sync_active_keymap(&s.cfg.keymap);
    let language = s.cfg.general.language;
    let (clashes, shadows) = crate::ui::shortcuts::conflicts(language);
    let mut check = col().key("shortcuts-check").w_full().gap(6.0);
    if let Some(target) = s.recording_key {
        let msg = match target {
            RecordingTarget::Leader => tr(
                language,
                "Recording Leader key… Press any modifier + key chord, or Esc to cancel.",
            ),
            RecordingTarget::Action(act) => tr(
                language,
                &format!(
                    "Recording shortcut for “{}”… Press any key or chord, or Esc to cancel.",
                    keymap::entry(act).what(language)
                ),
            ),
        };
        check = check.child(status(k, "sc-recording-banner", Tone::Quiet, msg));
    }
    if clashes.is_empty() {
        check = check.child(status(
            k,
            "sc-check-ok",
            Tone::Good,
            tr(language, "No conflicts: every chord does one thing."),
        ));
    }
    for (i, words) in clashes.into_iter().enumerate() {
        check = check.child(status(k, &format!("sc-clash-{i}"), Tone::Bad, words));
    }
    let mut out = vec![check, leader_group(s, k)];
    for g in Group::ALL {
        let mut rows: Vec<El> = keymap::ENTRIES
            .iter()
            .enumerate()
            .filter(|(_, e)| e.group == g)
            .map(|(i, e)| interactive_entry_row(s, k, e, format!("sc-r{i}")))
            .collect();
        if rows.is_empty() {
            continue;
        }
        if g == Group::Terminal {
            let note = if KeyPlatform::current() == KeyPlatform::Mac {
                tr(
                    language,
                    "Every other key goes to the shell; ⌘ shortcuts stay the app's.",
                )
            } else {
                tr(
                    language,
                    "Every other key goes to the shell. Add Shift for the app's Ctrl shortcuts (Ctrl+Shift+K for the palette).",
                )
            };
            rows.push(
                text(note)
                    .key("sc-terminal-note")
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .wrap()
                    .w_full()
                    .py(8.0),
            );
        }
        out.push(group(
            k,
            &format!("shortcuts-{g:?}"),
            &g.title(language),
            rows,
        ));
    }
    let notes: Vec<El> = shadows
        .into_iter()
        .chain(crate::ui::shortcuts::field_notes(language))
        .enumerate()
        .map(|(i, words)| {
            text(words)
                .key(format!("sc-note-{i}"))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full()
                .py(8.0)
        })
        .collect();
    if !notes.is_empty() {
        out.push(group(
            k,
            "shortcuts-notes",
            tr_ref(language, "Good to know"),
            notes,
        ));
    }
    out
}
