//! Settings: Agents, Appearance, General, Themes, About.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rune::{col, el, row, text, El, ImageCache, SurfaceFit, TextEdit, Transition, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, Kit, SegmentedControl, Switch,
    TextInput,
};

use super::agents::Standing;
use super::art;
use super::copy::{tr, tr_ref};
use super::{heading, muted, page_frame_w, UiState};
use crate::agent_fields::{fields, Field, Kind};
use crate::branding::APP_NAME;
use crate::config::{AgentConfigs, ConfigEdit, Density, Language, Scheme};
use crate::model::commands::Command;
use crate::platform::editors::Editor;
use crate::theme::gallery::IndexEntry;
use crate::theme::service::Job;
use crate::theme::store::ThemeEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Agents,
    Appearance,
    General,
    Voice,
    Themes,
    About,
}

fn tabs(language: Language) -> [(&'static str, SettingsTab); 6] {
    [
        (tr_ref(language, "Agents", "Agents"), SettingsTab::Agents),
        (
            tr_ref(language, "Appearance", "外观"),
            SettingsTab::Appearance,
        ),
        (tr_ref(language, "General", "通用"), SettingsTab::General),
        (tr_ref(language, "Voice", "语音"), SettingsTab::Voice),
        (tr_ref(language, "Themes", "主题"), SettingsTab::Themes),
        (tr_ref(language, "About", "关于"), SettingsTab::About),
    ]
}

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
        .py(8.0)
        .child(copy)
        .child(control)
}

fn section(k: &Kit, key: &str, title: &str, children: Vec<El>) -> El {
    let mut card = Card::new().title(title.to_string(), None);
    for child in children {
        card = card.child(child);
    }
    surface(k, card.build(k)).key(key.to_string())
}

/// A card on its own surface: a translucent sheet with a hairline rim, as
/// Home's cards. (Pages read the kit only; the sheet is a layer colour at a
/// fixed alpha, so it keeps reading well under reduce transparency.)
fn surface(k: &Kit, card: El) -> El {
    card.px(6.0)
        .py(4.0)
        .radius(crate::theme::look::CARD_RADIUS)
        .fill(k.c.layer_01.alpha(0.6))
        .border(1.0, k.c.border_muted)
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

fn standing_badge(k: &Kit, standing: Standing, word: &str) -> El {
    let variant = match standing {
        Standing::Usable => BadgeVariant::Accent,
        Standing::Checking => BadgeVariant::Subtle,
        Standing::Disabled => BadgeVariant::Neutral,
        _ => BadgeVariant::Neutral,
    };
    Badge::new(word.to_string()).variant(variant).build(k)
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
                .py(4.0)
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
    let language = s.cfg.general.language;
    if s.agents.is_empty() {
        return vec![empty_note(
            k,
            tr_ref(language, "Looking for agents…", "正在发现 Agents…"),
            tr_ref(
                language,
                "Muqun probes the agents installed on this computer.",
                "Muqun 会探测这台电脑上已安装的 Agents。",
            ),
        )];
    }
    let mut out = vec![
        section(
            k,
            "agents-intro",
            tr_ref(language, "Installed agents", "已安装的 Agents"),
            vec![
                muted(
                    k,
                    tr(
                        language,
                        "Enable the agents you use. Field changes save as you type; toggles take effect after Refresh.",
                        "打开要用的 Agents。字段随输入保存，开关在刷新后生效。",
                    ),
                ),
                row().key("agents-toolbar").w_full().gap(8.0).child(
                    Button::new(tr_ref(language, "Rediscover agents", "重新发现"))
                        .variant(ButtonVariant::Outline)
                        .control_size(ControlSize::Regular)
                        .on_press(|s: &mut UiState| s.send(Command::Refresh))
                        .build(k)
                        .key("agents-refresh"),
                ),
            ],
        ),
    ];
    out.extend(
        super::agents::by_standing(&s.agents)
            .into_iter()
            .map(|(a, standing)| {
                let enabled = s.cfg.agents.enabled(&a.id);
                let id = a.id.clone();
                let brand = super::agents::color(&a.id, s.dark);
                let title = super::agents::name(&a.id, &s.agents);
                let standing_word = standing.word(language);
                let mut card = Card::new()
                    .accent(if enabled { brand } else { k.c.icon_faint })
                    .title(title, Some(super::agents::mark_icon(&a.id)))
                    .child(
                        row()
                            .key(format!("stand-{}", a.id))
                            .gap(8.0)
                            .center_y()
                            .child(standing_badge(k, standing, standing_word))
                            .child(muted(
                                k,
                                a.version
                                    .as_ref()
                                    .map(|v| format!("v{v}"))
                                    .unwrap_or_default(),
                            )),
                    )
                    .child(setting(
                        k,
                        &format!("on-{}", a.id),
                        tr_ref(language, "Enabled", "启用"),
                        "",
                        flag(
                            k,
                            &format!("on-{}", a.id),
                            enabled,
                            tr_ref(language, "Enabled", "启用"),
                            move |s| {
                                s.edit(ConfigEdit::AgentEnabled {
                                    agent: id.clone(),
                                    on: !enabled,
                                });
                                s.send(Command::Refresh);
                            },
                        ),
                    ));
                if let Some(reason) = &a.reason {
                    card = card.child(muted(k, reason.clone()));
                }
                for field in fields(&a.id) {
                    card = card.child(agent_field(s, k, &a.id, *field));
                }
                surface(k, card.build(k))
                    .key(format!("agent-{}", a.id))
                    .transition(Transition::opacity().combined(Transition::offset(0.0, 6.0)))
            }),
    );
    out
}

// ---- Appearance --------------------------------------------------------------------

fn appearance(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let ap = &s.cfg.appearance;
    let scheme = match ap.scheme {
        Scheme::Light => 0,
        Scheme::Dark => 1,
        Scheme::System => 2,
    };
    let density = usize::from(ap.density == Density::Compact);
    let (motion, glass) = (ap.reduced_motion, ap.reduced_transparency);
    vec![
        section(
            k,
            "appearance-theme",
            tr_ref(language, "Theme", "主题"),
            vec![setting(
                k,
                "theme-jump",
                tr_ref(language, "Active theme", "当前主题"),
                tr_ref(
                    language,
                    "Open the Themes tab for previews, install and import.",
                    "在「主题」页查看预览、安装与导入。",
                ),
                row()
                    .key("theme-jump-row")
                    .gap(8.0)
                    .center_y()
                    .child(
                        Badge::new(ap.theme_id.clone())
                            .variant(BadgeVariant::Subtle)
                            .build(k),
                    )
                    .child(
                        Button::new(tr_ref(language, "Browse themes", "浏览主题"))
                            .variant(ButtonVariant::Outline)
                            .control_size(ControlSize::Small)
                            .on_press(|s: &mut UiState| {
                                s.settings_tab = SettingsTab::Themes;
                                s.send(Command::ThemeRefresh);
                            })
                            .build(k)
                            .key("goto-themes"),
                    ),
            )],
        ),
        section(
            k,
            "appearance-card",
            tr_ref(language, "Look & feel", "外观与动效"),
            vec![
                setting(
                    k,
                    "scheme",
                    tr_ref(language, "Color scheme", "配色"),
                    tr_ref(
                        language,
                        "Follow the system or pin light or dark.",
                        "跟随系统，或固定浅色 / 深色。",
                    ),
                    choice(
                        k,
                        "scheme",
                        [
                            tr_ref(language, "Light", "浅色"),
                            tr_ref(language, "Dark", "深色"),
                            tr_ref(language, "System", "系统"),
                        ],
                        scheme,
                        |s, i| {
                            s.set_scheme([Scheme::Light, Scheme::Dark, Scheme::System][i]);
                        },
                    ),
                ),
                setting(
                    k,
                    "density",
                    tr_ref(language, "Density", "密度"),
                    tr_ref(language, "Row heights and spacing.", "行高与间距。"),
                    choice(
                        k,
                        "density",
                        [
                            tr_ref(language, "Comfortable", "舒适"),
                            tr_ref(language, "Compact", "紧凑"),
                        ],
                        density,
                        |s, i| {
                            s.edit(ConfigEdit::Density(
                                [Density::Comfortable, Density::Compact][i],
                            ));
                        },
                    ),
                ),
                setting(
                    k,
                    "motion",
                    tr_ref(language, "Reduce motion", "减少动态"),
                    tr_ref(
                        language,
                        "Large movements become short fades.",
                        "大幅动画改为短淡入淡出。",
                    ),
                    flag(
                        k,
                        "motion",
                        motion,
                        tr_ref(language, "Reduce motion", "减少动态"),
                        move |s| s.edit(ConfigEdit::ReducedMotion(!motion)),
                    ),
                ),
                setting(
                    k,
                    "glass",
                    tr_ref(language, "Reduce transparency", "减少透明"),
                    tr_ref(
                        language,
                        "Glass surfaces and cover art become solid.",
                        "玻璃表面与封面插图改为不透明。",
                    ),
                    flag(
                        k,
                        "glass",
                        glass,
                        tr_ref(language, "Reduce transparency", "减少透明"),
                        move |s| s.edit(ConfigEdit::ReducedTransparency(!glass)),
                    ),
                ),
            ],
        ),
    ]
}

// ---- General -----------------------------------------------------------------------

fn general(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let lang_at = usize::from(language == Language::ZhCn);
    let editor = TextEdit::new(s.cfg.general.editor_command.clone().unwrap_or_default());
    vec![
        section(
            k,
            "general-locale",
            tr_ref(language, "Language", "语言"),
            vec![setting(
                k,
                "language",
                tr_ref(language, "Interface language", "界面语言"),
                tr_ref(
                    language,
                    "Applies immediately to labels in this window.",
                    "立即作用于当前窗口文案。",
                ),
                choice(
                    k,
                    "language",
                    ["English", "简体中文"],
                    lang_at,
                    |s, i| {
                        s.edit(ConfigEdit::Language([Language::En, Language::ZhCn][i]));
                    },
                ),
            )],
        ),
        section(
            k,
            "general-workspace",
            tr_ref(language, "Workspace", "工作区"),
            vec![
                col()
                    .key("set-cwd")
                    .w_full()
                    .gap(4.0)
                    .py(4.0)
                    .child(
                        TextInput::new(&s.cwd_input)
                            .label(tr_ref(language, "Default folder", "默认文件夹"))
                            .placeholder(tr_ref(language, "Home directory", "用户主目录"))
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
                        tr(
                            language,
                            "Where New session starts when an agent has no remembered folder.",
                            "Agent 没有记住文件夹时，新建会话从这里开始。",
                        ),
                    )),
                setting(
                    k,
                    "open-in",
                    tr_ref(language, "Open in", "打开方式"),
                    tr_ref(
                        language,
                        "Where Open in editor sends a project folder.",
                        "「在编辑器中打开」使用的应用。",
                    ),
                    choice(
                        k,
                        "open-in",
                        ["VS Code", "Zed", "Cursor", "Terminal", "Files"],
                        Editor::ALL
                            .iter()
                            .position(|e| *e == s.cfg.general.editor)
                            .unwrap_or(0),
                        |s, i| s.edit(ConfigEdit::Editor(Editor::ALL[i])),
                    ),
                ),
                col()
                    .key("set-editor")
                    .w_full()
                    .gap(4.0)
                    .py(4.0)
                    .child(
                        TextInput::new(&s.agent_inputs.get("editor").cloned().unwrap_or(editor))
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
                            })
                            .build(k)
                            .key("in-editor"),
                    )
                    .child(muted(
                        k,
                        tr(
                            language,
                            "Optional override for Open in editor.",
                            "可选，覆盖「在编辑器中打开」的命令。",
                        ),
                    )),
            ],
        ),
        notifications(s, k),
    ]
}

/// The App's "Gateway notifications": an approval, a question or a finished
/// turn in a session that is not on screen raises a system notification.
fn notifications(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let on = s.cfg.notifications.enabled;
    let title = tr_ref(language, "System notifications", "系统通知");
    section(
        k,
        "general-notify",
        tr_ref(language, "Notifications", "通知"),
        vec![setting(
            k,
            "notify",
            title,
            tr_ref(
                language,
                "Approvals, questions and finished turns in sessions you are not looking at.",
                "不在眼前的会话需要审批、提问或完成时提醒你。",
            ),
            flag(k, "notify", on, title, move |s| {
                s.edit(ConfigEdit::Notifications(!on))
            }),
        )],
    )
}

// ---- Themes ------------------------------------------------------------------------

/// Card picture size (8×5, matching gallery thumbnails).
const TILE_W: f32 = 236.0;
const TILE_H: f32 = 148.0;

fn swatch(k: &Kit, key: &str, hex: &str) -> El {
    let color = rune::color::hex(hex).unwrap_or(k.c.layer_02);
    el().key(key.to_string())
        .size(14.0, 14.0)
        .radius(k.radius.sm)
        .fill(color)
}

fn swatches(
    k: &Kit,
    id: &str,
    light: &crate::theme::store::Swatch,
    dark: &crate::theme::store::Swatch,
) -> El {
    row()
        .key(format!("sw-{id}"))
        .gap(6.0)
        .center_y()
        .child(
            row()
                .key("light")
                .gap(2.0)
                .child(swatch(k, "l-bg", &light.background))
                .child(swatch(k, "l-sf", &light.surface))
                .child(swatch(k, "l-pr", &light.primary))
                .child(swatch(k, "l-tx", &light.text)),
        )
        .child(
            row()
                .key("dark")
                .gap(2.0)
                .child(swatch(k, "d-bg", &dark.background))
                .child(swatch(k, "d-sf", &dark.surface))
                .child(swatch(k, "d-pr", &dark.primary))
                .child(swatch(k, "d-tx", &dark.text)),
        )
}

fn job_label(language: Language, job: &Job) -> String {
    match job {
        Job::Downloading { read, total } if *total > 0 => tr(
            language,
            &format!("Downloading {}%", read * 100 / total),
            &format!("下载中 {}%", read * 100 / total),
        ),
        Job::Downloading { .. } => tr(language, "Downloading…", "下载中…"),
        Job::Verifying => tr(language, "Verifying…", "校验中…"),
        Job::Installing => tr(language, "Installing…", "安装中…"),
        Job::Done { verified: true } => tr(
            language,
            "Installed (checksum verified)",
            "已安装（校验通过）",
        ),
        Job::Done { verified: false } => tr(language, "Installed", "已安装"),
        Job::Failed { message } => tr(
            language,
            &format!("Failed: {message}"),
            &format!("失败：{message}"),
        ),
    }
}

fn thumb_from_path(k: &Kit, images: Option<&ImageCache>, path: Option<&Path>, key: &str) -> El {
    if let (Some(images), Some(path)) = (images, path) {
        if let Ok(bytes) = std::fs::read(path) {
            return art::picture(images, Arc::<[u8]>::from(bytes), SurfaceFit::Fill)
                .key(key.to_string())
                .w(TILE_W)
                .h(TILE_H);
        }
    }
    el().key(key.to_string())
        .w(TILE_W)
        .h(TILE_H)
        .fill(k.c.layer_02)
}

struct ThemeTile {
    key: String,
    name: String,
    about: String,
    thumb: El,
    palette: Option<El>,
    status: Option<El>,
    actions: Vec<El>,
    current: bool,
}

fn theme_tile(k: &Kit, tile: ThemeTile) -> El {
    let border = if tile.current {
        k.c.border_focus
    } else {
        k.c.border_weak
    };
    let mut body = col()
        .key("meta")
        .w_full()
        .gap(6.0)
        .px(12.0)
        .pt(10.0)
        .pb(12.0)
        .child(
            text(tile.name)
                .font(k.ty.body_medium)
                .color(k.c.text_base)
                .truncate(),
        )
        .child(
            text(tile.about)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate(),
        );
    if let Some(palette) = tile.palette {
        body = body.child(palette);
    }
    if let Some(status) = tile.status {
        body = body.child(status);
    }
    if !tile.actions.is_empty() {
        body = body.child(
            row()
                .key("actions")
                .gap(8.0)
                .center_y()
                .children(tile.actions),
        );
    }
    col()
        .key(tile.key)
        .w(TILE_W)
        .radius(k.radius.md)
        .fill(k.c.bg_base)
        .border(1.0, border)
        .clip_rounded(k.radius.md)
        .transition(
            Transition::opacity()
                .combined(Transition::offset(0.0, 10.0))
                .combined(Transition::scale(0.98)),
        )
        .child(tile.thumb)
        .child(body)
}

fn library_tile(s: &UiState, k: &Kit, images: Option<&ImageCache>, entry: &ThemeEntry) -> El {
    let language = s.cfg.general.language;
    let current = entry.id == s.cfg.appearance.theme_id;
    let mut about = vec![format!("v{}", entry.version)];
    about.extend(entry.author.clone());
    let scheme = s.cfg.appearance.scheme;
    let (apply_id, remove_id) = (entry.id.clone(), entry.id.clone());
    let mut actions = vec![if current {
        Badge::new(tr_ref(language, "Current", "当前"))
            .variant(BadgeVariant::Accent)
            .build(k)
    } else {
        Button::new(tr_ref(language, "Apply", "应用"))
            .variant(ButtonVariant::Submit)
            .control_size(ControlSize::Small)
            .on_press(move |s: &mut UiState| {
                s.send(Command::ThemeApply(apply_id.clone(), scheme));
            })
            .build(k)
            .key(format!("apply-{}", entry.id))
    }];
    if entry.origin.removable() {
        actions.push(
            Button::new(tr_ref(language, "Remove", "移除"))
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| {
                    s.send(Command::ThemeRemove(remove_id.clone()));
                })
                .build(k)
                .key(format!("remove-{}", entry.id)),
        );
    }
    let thumb = thumb_from_path(
        k,
        images,
        entry.thumbnail.as_deref(),
        &format!("thumb-lib-{}", entry.id),
    );
    let fallback = Some(swatches(k, &entry.id, &entry.light, &entry.dark));
    theme_tile(
        k,
        ThemeTile {
            key: format!("theme-{}", entry.id),
            name: entry.name.clone(),
            about: about.join(" · "),
            thumb,
            palette: fallback,
            status: entry
                .description
                .as_ref()
                .filter(|d| !d.is_empty())
                .map(|d| muted(k, d.clone())),
            actions,
            current,
        },
    )
}

fn gallery_tile(s: &UiState, k: &Kit, images: Option<&ImageCache>, entry: &IndexEntry) -> El {
    let language = s.cfg.general.language;
    let installed = s.theme_library.iter().any(|t| t.id == entry.id);
    let job = s.theme_jobs.get(&entry.id);
    let running = job.is_some_and(|j| {
        matches!(
            j,
            Job::Downloading { .. } | Job::Verifying | Job::Installing
        )
    });
    let mut about = vec![format!("v{}", entry.version)];
    about.extend(entry.author.clone());
    about.push(format!("{} KB", entry.bytes / 1024));
    let preview = s.theme_previews.get(&entry.id).map(PathBuf::as_path);
    let thumb = thumb_from_path(k, images, preview, &format!("thumb-gal-{}", entry.id));
    let id = entry.id.clone();
    let actions = vec![Button::new(if installed {
        tr_ref(language, "Reinstall", "重新安装")
    } else {
        tr_ref(language, "Install", "安装")
    })
    .variant(if installed {
        ButtonVariant::Outline
    } else {
        ButtonVariant::Submit
    })
    .control_size(ControlSize::Small)
    .disabled(running)
    .on_press(move |s: &mut UiState| s.send(Command::ThemeInstall(id.clone())))
    .build(k)
    .key(format!("install-{}", entry.id))];
    let status = job.map(|job| muted(k, job_label(language, job)));
    theme_tile(
        k,
        ThemeTile {
            key: format!("gallery-{}", entry.id),
            name: entry.name.clone(),
            about: about.join(" · "),
            thumb,
            palette: None,
            status: status.or_else(|| {
                entry
                    .description
                    .as_ref()
                    .filter(|d| !d.is_empty())
                    .map(|d| muted(k, d.clone()))
            }),
            actions,
            current: false,
        },
    )
}

fn theme_grid(key: &str, tiles: Vec<El>) -> El {
    row()
        .key(key.to_string())
        .w_full()
        .gap(14.0)
        .flex_wrap()
        .children(tiles)
}

fn section_head(k: &Kit, title: &str, detail: &str, trailing: Option<El>) -> El {
    let mut copy = col()
        .grow(1.0)
        .shrink()
        .gap(2.0)
        .child(heading(k, title.to_string()));
    if !detail.is_empty() {
        copy = copy.child(muted(k, detail));
    }
    let mut row = row()
        .key(format!("head-{title}"))
        .w_full()
        .gap(12.0)
        .center_y()
        .pt(4.0)
        .child(copy);
    if let Some(t) = trailing {
        row = row.child(t);
    }
    row
}

fn current_theme_hero(s: &UiState, k: &Kit, images: Option<&ImageCache>) -> El {
    let language = s.cfg.general.language;
    let id = &s.cfg.appearance.theme_id;
    let entry = s.theme_library.iter().find(|t| t.id == *id);
    let name = entry.map(|t| t.name.as_str()).unwrap_or(id.as_str());
    let about = entry
        .map(|t| {
            let mut bits = vec![format!("v{}", t.version)];
            bits.extend(t.author.clone());
            bits.join(" · ")
        })
        .unwrap_or_default();
    let thumb_path = entry.and_then(|t| t.thumbnail.as_deref());
    let thumb = if let (Some(images), Some(path)) = (images, thumb_path) {
        if let Ok(bytes) = std::fs::read(path) {
            art::picture(images, Arc::<[u8]>::from(bytes), SurfaceFit::Fill)
                .key("current-thumb")
                .w(320.0)
                .h(200.0)
                .radius(k.radius.md)
                .clip_rounded(k.radius.md)
        } else {
            el().key("current-thumb")
                .w(320.0)
                .h(200.0)
                .radius(k.radius.md)
                .fill(k.c.layer_02)
        }
    } else {
        el().key("current-thumb")
            .w(320.0)
            .h(200.0)
            .radius(k.radius.md)
            .fill(k.c.layer_02)
    };
    let mut meta = col()
        .key("current-meta")
        .grow(1.0)
        .gap(8.0)
        .child(
            Badge::new(tr_ref(language, "Current theme", "当前主题"))
                .variant(BadgeVariant::Accent)
                .build(k),
        )
        .child(
            text(name.to_string())
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        );
    if !about.is_empty() {
        meta = meta.child(muted(k, about));
    }
    if let Some(entry) = entry {
        meta = meta.child(swatches(k, &entry.id, &entry.light, &entry.dark));
        if let Some(d) = entry.description.as_ref().filter(|d| !d.is_empty()) {
            meta = meta.child(muted(k, d.clone()));
        }
    } else {
        meta = meta.child(muted(
            k,
            tr(
                language,
                "Applies to the light and dark variants alike.",
                "浅色与深色变体共用同一主题。",
            ),
        ));
    }
    row()
        .key("theme-current")
        .w_full()
        .gap(20.0)
        .items_center()
        .px(14.0)
        .py(14.0)
        .radius(k.radius.md)
        .fill(k.c.bg_base)
        .border(1.0, k.c.border_weak)
        .child(thumb)
        .child(meta)
        .transition(Transition::opacity().combined(Transition::offset(0.0, 8.0)))
}

fn themes(s: &UiState, k: &Kit, images: Option<&ImageCache>) -> Vec<El> {
    let language = s.cfg.general.language;
    let mut out = vec![
        current_theme_hero(s, k, images),
        section_head(
            k,
            tr_ref(language, "Library", "已安装"),
            tr_ref(
                language,
                "Themes on this computer. Apply one to restyle the window.",
                "本机已有主题。应用后立即更换窗口外观。",
            ),
            Some(
                Button::new(tr_ref(language, "Refresh", "刷新"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| s.send(Command::ThemeRefresh))
                    .build(k)
                    .key("theme-refresh"),
            ),
        ),
    ];
    if s.theme_library.is_empty() {
        out.push(empty_note(
            k,
            tr_ref(language, "No themes yet", "还没有主题"),
            tr_ref(
                language,
                "Import a package or install one from the gallery below.",
                "导入主题包，或从下方图库安装。",
            ),
        ));
    } else {
        out.push(theme_grid(
            "library-grid",
            s.theme_library
                .iter()
                .map(|t| library_tile(s, k, images, t))
                .collect(),
        ));
    }

    out.push(section_head(
        k,
        tr_ref(language, "Import", "导入"),
        tr_ref(
            language,
            "Paste a .muqun-theme link or open a package from disk.",
            "粘贴 .muqun-theme 链接，或从磁盘打开主题包。",
        ),
        None,
    ));
    out.push(
        row()
            .key("theme-import")
            .w_full()
            .gap(8.0)
            .center_y()
            .flex_wrap()
            .px(12.0)
            .py(12.0)
            .radius(k.radius.md)
            .fill(k.c.bg_base)
            .border(1.0, k.c.border_weak)
            .child(
                TextInput::new(&s.theme_url)
                    .label(tr_ref(language, "Theme link", "主题链接"))
                    .placeholder("https://… .muqun-theme")
                    .on_edit(|s: &mut UiState| &mut s.theme_url)
                    .on_submit(import_url)
                    .build(k)
                    .key("theme-url")
                    .grow(1.0)
                    .min_w(240.0),
            )
            .child(
                Button::new(tr_ref(language, "Import link", "导入链接"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Regular)
                    .disabled(s.theme_url.text.trim().is_empty())
                    .on_press(|s: &mut UiState| import_url(s, &mut Tx::default()))
                    .build(k)
                    .key("theme-import-url"),
            )
            .child(
                Button::new(tr_ref(language, "Import file…", "导入文件…"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Regular)
                    .on_press(|_: &mut UiState| crate::platform::dialogs::import_theme_file())
                    .build(k)
                    .key("theme-import-file"),
            ),
    );

    out.push(section_head(
        k,
        tr_ref(language, "Gallery", "图库"),
        tr_ref(
            language,
            "Published themes from muqun.dev, with preview cards.",
            "来自 muqun.dev 的主题，带预览图。",
        ),
        None,
    ));
    let gallery = &s.theme_gallery;
    if gallery.loading && gallery.entries.is_empty() {
        out.push(muted(
            k,
            tr_ref(language, "Loading the gallery…", "正在加载图库…"),
        ));
    }
    if let Some(err) = &gallery.error {
        out.push(
            Card::new()
                .accent(k.c.danger.icon)
                .title(tr_ref(language, "Gallery unavailable", "图库不可用"), None)
                .description(err.clone())
                .action(
                    Button::new(tr_ref(language, "Retry", "重试"))
                        .variant(ButtonVariant::Outline)
                        .control_size(ControlSize::Small)
                        .on_press(|s: &mut UiState| s.send(Command::ThemeRefresh))
                        .build(k)
                        .key("gallery-retry"),
                )
                .build(k)
                .key("gallery-error"),
        );
    }
    if let Some(why) = &gallery.stale {
        out.push(muted(
            k,
            tr(
                language,
                &format!("Showing the saved list: {why}"),
                &format!("显示已保存列表：{why}"),
            ),
        ));
    }
    if !gallery.entries.is_empty() {
        out.push(theme_grid(
            "gallery-grid",
            gallery
                .entries
                .iter()
                .take(s.gallery_limit)
                .map(|e| gallery_tile(s, k, images, e))
                .collect(),
        ));
    }
    if gallery.entries.len() > s.gallery_limit {
        out.push(
            Button::new(tr_ref(language, "Show more", "显示更多"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Regular)
                .on_press(|s: &mut UiState| s.gallery_limit += 8)
                .build(k)
                .key("gallery-more"),
        );
    }
    out
}

fn import_url(s: &mut UiState, _: &mut Tx) {
    let url = s.theme_url.text.trim().to_string();
    if url.is_empty() {
        return;
    }
    s.theme_url = TextEdit::new("");
    s.send(Command::ThemeImportUrl(url));
}

// ---- About -------------------------------------------------------------------------

fn about(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let version = env!("CARGO_PKG_VERSION");
    let (config, state) =
        crate::platform::directories().unwrap_or_else(|_| (PathBuf::new(), PathBuf::new()));
    let hub = if s.hub_label.is_empty() {
        tr(language, "Starting…", "启动中…")
    } else {
        hub_label(language, &s.hub_label)
    };
    vec![
        section(
            k,
            "about-app",
            APP_NAME,
            vec![
                muted(k, format!("muqun-desktop {version}")),
                muted(
                    k,
                    tr(
                        language,
                        "Coding agents on this computer.",
                        "在这台电脑上使用编程 Agents。",
                    ),
                ),
                setting(
                    k,
                    "hub-mode",
                    tr_ref(language, "Hub", "连接"),
                    tr_ref(
                        language,
                        "How this window talks to agents.",
                        "当前窗口连接 Agents 的方式。",
                    ),
                    Badge::new(hub).variant(BadgeVariant::Subtle).build(k),
                ),
                setting(
                    k,
                    "theme-about",
                    tr_ref(language, "Theme", "主题"),
                    "",
                    Badge::new(s.cfg.appearance.theme_id.clone())
                        .variant(BadgeVariant::Subtle)
                        .build(k),
                ),
            ],
        ),
        section(
            k,
            "about-paths",
            tr_ref(language, "Files", "文件位置"),
            vec![
                path_row(
                    k,
                    "config",
                    tr_ref(language, "Config", "配置"),
                    config.display().to_string(),
                ),
                path_row(
                    k,
                    "state",
                    tr_ref(language, "State", "状态"),
                    state.display().to_string(),
                ),
            ],
        ),
    ]
}

fn path_row(k: &Kit, key: &str, title: &str, path: String) -> El {
    col()
        .key(format!("path-{key}"))
        .w_full()
        .gap(2.0)
        .py(6.0)
        .child(
            text(title.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_base),
        )
        .child(text(path).font(k.ty.mono).color(k.c.text_muted).wrap())
}

fn hub_label(language: Language, raw: &str) -> String {
    match raw {
        "local gateway" => tr(language, "Local gateway", "本机 Gateway"),
        "standalone" => tr(language, "Standalone", "独立运行"),
        other => other.to_string(),
    }
}

fn empty_note(k: &Kit, title: &str, detail: &str) -> El {
    Card::new()
        .accent(k.c.icon_faint)
        .title(title.to_string(), None)
        .description(detail.to_string())
        .build(k)
        .key("settings-empty")
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let tab_defs = tabs(language);
    let labels: Vec<&str> = tab_defs.iter().map(|(l, _)| *l).collect();
    let at = tab_defs.iter().position(|(_, t)| *t == s.settings_tab);
    let images = s.images.as_ref();
    let wide = s.settings_tab == SettingsTab::Themes;
    let body = match s.settings_tab {
        SettingsTab::Agents => agents(s, k),
        SettingsTab::Appearance => appearance(s, k),
        SettingsTab::General => general(s, k),
        SettingsTab::Voice => super::voice::settings(s, k),
        SettingsTab::Themes => themes(s, k, images),
        SettingsTab::About => about(s, k),
    };
    let subtitle = match s.settings_tab {
        SettingsTab::Agents => tr_ref(
            language,
            "Discovery, enablement, and agent-specific fields.",
            "发现、启用与各 Agent 专属字段。",
        ),
        SettingsTab::Appearance => tr_ref(
            language,
            "Color scheme, density, and motion preferences.",
            "配色、密度与动效偏好。",
        ),
        SettingsTab::General => tr_ref(
            language,
            "Language, default folder, editor and notifications.",
            "语言、默认文件夹、编辑器与通知。",
        ),
        SettingsTab::Voice => super::voice::subtitle(language),
        SettingsTab::Themes => tr_ref(
            language,
            "Library, import, and gallery with preview cards.",
            "已安装、导入与带预览的图库。",
        ),
        SettingsTab::About => tr_ref(
            language,
            "Version, hub mode, and data locations.",
            "版本、连接方式与数据位置。",
        ),
    };
    let mut page = page_frame_w(
        k,
        "settings",
        tr_ref(language, "Settings", "设置"),
        Some(subtitle),
        if wide { 920.0 } else { 760.0 },
    )
    .child(
        SegmentedControl::new(&labels, at)
            .fit()
            .label(tr_ref(language, "Settings", "设置"))
            .on_pick(move |s: &mut UiState, i, _: &mut Tx| {
                s.settings_tab = tab_defs[i].1;
                if s.settings_tab == SettingsTab::Themes {
                    s.send(Command::ThemeRefresh);
                }
            })
            .build(k)
            .key("settings-tabs"),
    );
    for child in body {
        page = page.child(child);
    }
    super::page_stage(s, k, "settings", page)
}
