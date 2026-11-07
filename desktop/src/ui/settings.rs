//! Settings: Agents, Appearance, General, Themes.

use std::path::PathBuf;

use rune::{col, row, text, El, ScrollSpec, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, Kit, ScrollBar,
    SegmentedControl, Switch, TextInput,
};

use super::{heading, muted, UiState};
use crate::agent_fields::{fields, Field, Kind};
use crate::config::{AgentConfigs, ConfigEdit, Density, Language, Scheme};
use crate::model::commands::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Agents,
    Appearance,
    General,
    Themes,
}

const TABS: [(&str, SettingsTab); 4] = [
    ("Agents", SettingsTab::Agents),
    ("Appearance", SettingsTab::Appearance),
    ("General", SettingsTab::General),
    ("Themes", SettingsTab::Themes),
];

/// A titled setting with its control at the trailing edge.
fn setting(k: &Kit, key: &str, title: &str, detail: &str, control: El) -> El {
    let mut copy = col()
        .grow(1.0)
        .shrink()
        .gap(2.0)
        .child(text(title.to_string()).font(k.ty.ui).color(k.c.text_base));
    if !detail.is_empty() {
        copy = copy.child(muted(k, detail));
    }
    row()
        .key(format!("set-{key}"))
        .w_full()
        .gap(16.0)
        .center_y()
        .py(6.0)
        .child(copy)
        .child(control)
}

fn choice<const N: usize>(
    k: &Kit,
    key: &str,
    labels: [&str; N],
    at: usize,
    pick: impl Fn(&mut UiState, usize) + Clone + 'static,
) -> El {
    SegmentedControl::new(&labels, Some(at))
        .fit()
        .label(key.to_string())
        .on_pick(move |s: &mut UiState, i, _: &mut Tx| pick(s, i))
        .build(k)
        .key(format!("seg-{key}"))
}

fn flag(k: &Kit, key: &str, on: bool, label: &str, f: impl Fn(&mut UiState) + 'static) -> El {
    Switch::new(on)
        .label(label)
        .hide_label()
        .on_toggle(f)
        .build(k)
        .key(format!("sw-{key}"))
}

// ---- Agents ------------------------------------------------------------------------

fn set_field(s: &mut UiState, agent: &str, field: Field, value: String) {
    let slot = format!("{agent}.{}", field.key());
    match AgentConfigs::check(agent, field, &value) {
        Ok(()) => {
            s.agent_errors.remove(&slot);
            s.edit(ConfigEdit::AgentValue {
                agent: agent.to_string(),
                field,
                value,
            });
        }
        Err(err) => {
            s.agent_errors.insert(slot, err.to_string());
        }
    }
}

fn agent_field(s: &UiState, k: &Kit, agent: &str, field: Field) -> El {
    let slot = format!("{agent}.{}", field.key());
    let stored = s.cfg.agents.value(agent, field);
    let label = field.key().replace('_', " ");
    let (a, f) = (agent.to_string(), field);
    match field.kind() {
        Kind::Flag => {
            let on = stored == "true";
            setting(
                k,
                &slot,
                &label,
                "",
                flag(k, &slot, on, &label, move |s| {
                    set_field(s, &a, f, (!on).to_string())
                }),
            )
        }
        Kind::Choice(options) => {
            let at = options.iter().position(|o| *o == stored).unwrap_or(0);
            let control = choice_dyn(k, &slot, options, at, move |s, i| {
                set_field(s, &a, f, options[i].to_string())
            });
            setting(k, &slot, &label, "", control)
        }
        Kind::Text | Kind::Number => {
            let edit = s
                .agent_inputs
                .get(&slot)
                .cloned()
                .unwrap_or_else(|| TextEdit::new(stored));
            let (key_in, key_ch, agent_in) = (slot.clone(), slot.clone(), agent.to_string());
            let mut block = col()
                .key(format!("field-{slot}"))
                .w_full()
                .gap(4.0)
                .child(
                    text(label.clone())
                        .font(k.ty.small_medium)
                        .color(k.c.text_base),
                )
                .child(
                    TextInput::new(&edit)
                        .label(label.clone())
                        .placeholder(label.clone())
                        .invalid(s.agent_errors.contains_key(&slot))
                        .on_edit(move |s: &mut UiState| {
                            let stored = s.cfg.agents.value(&agent_in, f);
                            s.agent_inputs
                                .entry(key_in.clone())
                                .or_insert_with(|| TextEdit::new(stored))
                        })
                        .on_change(move |s: &mut UiState, _: &mut Tx| {
                            let value = s
                                .agent_inputs
                                .get(&key_ch)
                                .map(|e| e.text.clone())
                                .unwrap_or_default();
                            set_field(s, &a, f, value);
                        })
                        .build(k)
                        .key(format!("in-{slot}")),
                );
            if let Some(err) = s.agent_errors.get(&slot) {
                block = block.child(
                    text(err.clone())
                        .font(k.ty.small)
                        .color(k.c.danger.fg)
                        .wrap(),
                );
            }
            block
        }
    }
}

fn choice_dyn(
    k: &Kit,
    key: &str,
    options: &'static [&'static str],
    at: usize,
    pick: impl Fn(&mut UiState, usize) + Clone + 'static,
) -> El {
    SegmentedControl::new(options, Some(at))
        .fit()
        .label(key.to_string())
        .on_pick(move |s: &mut UiState, i, _: &mut Tx| pick(s, i))
        .build(k)
        .key(format!("seg-{key}"))
}

fn agents(s: &UiState, k: &Kit) -> Vec<El> {
    if s.agents.is_empty() {
        return vec![muted(k, "No agents discovered yet.")];
    }
    s.agents
        .iter()
        .map(|a| {
            let enabled = s.cfg.agents.enabled(&a.id);
            let id = a.id.clone();
            let mut card = Card::new()
                .accent(if enabled {
                    k.c.success.icon
                } else {
                    k.c.icon_faint
                })
                .title(a.name.clone(), None)
                .description(format!(
                    "{:?}{}",
                    a.status,
                    a.version
                        .as_ref()
                        .map(|v| format!(" · {v}"))
                        .unwrap_or_default()
                ))
                .child(setting(
                    k,
                    &format!("on-{}", a.id),
                    "Enabled",
                    "",
                    flag(k, &format!("on-{}", a.id), enabled, "Enabled", move |s| {
                        s.edit(ConfigEdit::AgentEnabled {
                            agent: id.clone(),
                            on: !enabled,
                        });
                        s.send(Command::Refresh);
                    }),
                ));
            if let Some(reason) = &a.reason {
                card = card.child(muted(k, reason.clone()));
            }
            for field in fields(&a.id) {
                card = card.child(agent_field(s, k, &a.id, *field));
            }
            card.build(k).key(format!("agent-{}", a.id))
        })
        .collect()
}

// ---- Appearance --------------------------------------------------------------------

fn appearance(s: &UiState, k: &Kit) -> Vec<El> {
    let ap = &s.cfg.appearance;
    let scheme = match ap.scheme {
        Scheme::Light => 0,
        Scheme::Dark => 1,
        Scheme::System => 2,
    };
    let density = usize::from(ap.density == Density::Compact);
    let (motion, glass) = (ap.reduced_motion, ap.reduced_transparency);
    vec![
        setting(
            k,
            "scheme",
            "Color scheme",
            "Follow the system or pin light or dark.",
            choice(k, "scheme", ["Light", "Dark", "System"], scheme, |s, i| {
                s.set_scheme([Scheme::Light, Scheme::Dark, Scheme::System][i]);
            }),
        ),
        setting(
            k,
            "density",
            "Density",
            "Row heights and spacing.",
            choice(k, "density", ["Comfortable", "Compact"], density, |s, i| {
                s.edit(ConfigEdit::Density(
                    [Density::Comfortable, Density::Compact][i],
                ));
            }),
        ),
        setting(
            k,
            "motion",
            "Reduce motion",
            "Large movements become short fades.",
            flag(k, "motion", motion, "Reduce motion", move |s| {
                s.edit(ConfigEdit::ReducedMotion(!motion))
            }),
        ),
        setting(
            k,
            "glass",
            "Reduce transparency",
            "Glass surfaces become solid.",
            flag(k, "glass", glass, "Reduce transparency", move |s| {
                s.edit(ConfigEdit::ReducedTransparency(!glass))
            }),
        ),
    ]
}

// ---- General -----------------------------------------------------------------------

fn general(s: &UiState, k: &Kit) -> Vec<El> {
    let language = usize::from(s.cfg.general.language == Language::ZhCn);
    let editor = TextEdit::new(s.cfg.general.editor_command.clone().unwrap_or_default());
    vec![
        setting(
            k,
            "language",
            "Language",
            "Applies the next time the window opens.",
            choice(
                k,
                "language",
                ["English", "简体中文"],
                language,
                |s, i| {
                    s.edit(ConfigEdit::Language([Language::En, Language::ZhCn][i]));
                },
            ),
        ),
        col()
            .key("set-cwd")
            .w_full()
            .gap(4.0)
            .child(
                TextInput::new(&s.cwd_input)
                    .label("Default folder")
                    .placeholder("Home directory")
                    .on_edit(|s: &mut UiState| &mut s.cwd_input)
                    .on_change(|s: &mut UiState, _: &mut Tx| {
                        let text = s.cwd_input.text.trim().to_string();
                        s.edit(ConfigEdit::DefaultCwd(
                            (!text.is_empty()).then(|| PathBuf::from(text)),
                        ));
                    })
                    .build(k)
                    .key("in-cwd"),
            )
            .child(muted(
                k,
                "Where New session starts when an agent has no remembered folder.",
            )),
        col()
            .key("set-editor")
            .w_full()
            .gap(4.0)
            .child(
                TextInput::new(&s.agent_inputs.get("editor").cloned().unwrap_or(editor))
                    .label("Editor command")
                    .placeholder("code, zed, …")
                    .on_edit(|s: &mut UiState| {
                        let stored = s.cfg.general.editor_command.clone().unwrap_or_default();
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
                    })
                    .build(k)
                    .key("in-editor"),
            )
            .child(muted(k, "What Open in editor runs.")),
    ]
}

// ---- Themes ------------------------------------------------------------------------

fn themes(s: &UiState, k: &Kit) -> Vec<El> {
    vec![
        setting(
            k,
            "theme",
            "Current theme",
            "The theme library is not available in this build yet; the neutral kit theme is used.",
            Badge::new(s.cfg.appearance.theme_id.clone())
                .variant(BadgeVariant::Subtle)
                .build(k),
        ),
        row().key("theme-actions").gap(8.0).child(
            Button::new("Refresh themes")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .on_press(|s: &mut UiState| s.send(Command::ThemeRefresh))
                .build(k)
                .key("theme-refresh"),
        ),
    ]
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let labels: Vec<&str> = TABS.iter().map(|(l, _)| *l).collect();
    let at = TABS.iter().position(|(_, t)| *t == s.settings_tab);
    let body = match s.settings_tab {
        SettingsTab::Agents => agents(s, k),
        SettingsTab::Appearance => appearance(s, k),
        SettingsTab::General => general(s, k),
        SettingsTab::Themes => themes(s, k),
    };
    col()
        .key("settings")
        .w_full()
        .gap(16.0)
        .px(24.0)
        .py(24.0)
        .child(heading(k, "Settings"))
        .child(
            SegmentedControl::new(&labels, at)
                .fit()
                .label("Settings")
                .on_pick(|s: &mut UiState, i, _: &mut Tx| {
                    s.settings_tab = TABS[i].1;
                    if s.settings_tab == SettingsTab::Themes {
                        s.send(Command::ThemeRefresh);
                    }
                })
                .build(k)
                .key("settings-tabs"),
        )
        .children(body)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k)
}
