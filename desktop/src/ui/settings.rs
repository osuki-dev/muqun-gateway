//! The Settings page (⚙ in the sidebar, Ctrl/Cmd+,): four tabs.
//!
//! * **Agents** -- one section per agent: an enable switch and the fields of
//!   its gateway config block (`agent_fields.rs` holds the table and the
//!   validation). Changes are written to `config.json` at once but only take
//!   effect when the desktop next starts, which the page says.
//! * **Appearance** -- reading density/motion/glass, then themes (gallery,
//!   import file/link, apply), live.
//! * **General** -- language (live), the default folder, the editor command
//!   for later, and the "close to tray" placeholder.
//! * **About** -- version and where the files are.
//!
//! Every edit is a `Command::Edit`; the core applies it in memory and writes
//! the file after 500 ms of quiet. The inputs hold their own text in keyed
//! state, so switching tabs or rebuilding the page never loses what was typed.

use blinc_cn::components::input::InputBuilder;
use blinc_cn::components::select::SelectBuilder;
use blinc_cn::prelude::{
    button, input, switch, tabs, ButtonSize, ButtonVariant, InputSize, State, TabsSize,
};
use blinc_core::BlincContextState;
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_input::{text_input_data, SharedTextInputData};

use super::agents::{self, Standing};
use super::text::{mono_text, ui_text};
use super::App;
use crate::agent_fields::{self, Field, Kind};
use crate::config::{AgentConfigs, ConfigEdit, Density, Language};
use crate::hub::AGENT_PREFERENCE;
use crate::model::commands::Command;

const PAGE_WIDTH: f32 = 720.0;
/// Header plus the tab strip, which the tab content's height leaves room for.
const CHROME: f32 = 112.0;

/// One string in the current language.
fn tr(app: &App, en: &'static str, zh: &'static str) -> &'static str {
    match app.strings.language() {
        Language::En => en,
        Language::ZhCn => zh,
    }
}

pub fn page(app: &App, width: f32, height: f32) -> Div {
    let tab = app.bindings.settings_tab.clone();
    let body_height = (height - CHROME).max(120.0);
    let close = {
        let app = app.clone();
        button(tr(&app, "Close", "关闭"))
            .variant(ButtonVariant::Ghost)
            .size(ButtonSize::Small)
            .on_click(move |_| app.close_settings())
    };
    let (a, b, c, d) = (app.clone(), app.clone(), app.clone(), app.clone());
    let strip = tabs(&tab)
        .size(TabsSize::Medium)
        .tab("agents", tr(app, "Agents", "Agents"), move || {
            pane(body_height, agents_tab(&a))
        })
        .tab("appearance", tr(app, "Appearance", "外观"), move || {
            appearance_tab(&b, width, body_height)
        })
        .tab("general", tr(app, "General", "通用"), move || {
            pane(body_height, general_tab(&c))
        })
        .tab("about", tr(app, "About", "关于"), move || {
            pane(body_height, about_tab(&d))
        });
    div()
        .class("mq-settings")
        .w_full()
        .h(height)
        .flex_col()
        .child(
            div()
                .class("mq-settings-head")
                .w_full()
                .flex_row()
                .items_center()
                .child(
                    div().flex_1().child(
                        ui_text(tr(app, "Settings", "设置"))
                            .size(20.0)
                            .semibold()
                            .no_wrap(),
                    ),
                )
                .child(ui_text("Esc").size(11.0).no_wrap())
                .child(close),
        )
        .child(
            div()
                .class("mq-settings-tabs")
                .w_full()
                .flex_grow()
                .child(strip),
        )
}

/// A tab's content: scrolls inside the page, a readable width.
fn pane(height: f32, content: Div) -> Div {
    div().w_full().h(height).child(
        scroll().w_full().h(height).child(
            div()
                .class("mq-settings-pane")
                .w_full()
                .max_w(PAGE_WIDTH)
                .flex_col()
                .child(content),
        ),
    )
}

fn section_title(text: &str) -> Div {
    div()
        .class("mq-set-title")
        .child(ui_text(text.to_string()).size(12.0).semibold())
}

fn hint(text: impl Into<String>) -> Div {
    div()
        .class("mq-set-hint")
        .w_full()
        .child(ui_text(text).size(12.0))
}

/// A settings text field: small, fill-only (no stroke).
fn text_field(data: &SharedTextInputData) -> InputBuilder {
    input(data)
        .size(InputSize::Small)
        .idle_border_color(blinc_core::Color::TRANSPARENT)
}

// --- keyed inputs ---------------------------------------------------------

fn text_data(key: &str, initial: &str) -> SharedTextInputData {
    let initial = initial.to_string();
    BlincContextState::get()
        .use_state_keyed(key, move || {
            let data = text_input_data();
            if let Ok(mut d) = data.lock() {
                d.cursor = initial.chars().count();
                d.value = initial;
            }
            data
        })
        .get()
}

/// A line of red text that follows `error`, and nothing when it is empty.
fn error_line(key: String, error: State<String>) -> impl ElementBuilder {
    stateful_with_key::<NoState>(key)
        .deps([error.signal_id()])
        .on_state(move |_| {
            let message = error.get();
            if message.is_empty() {
                return div();
            }
            div()
                .class("mq-set-error")
                .child(ui_text(message).size(12.0))
        })
}

// --- Agents ---------------------------------------------------------------

fn agents_tab(app: &App) -> Div {
    let mut col = div().w_full().flex_col().class("mq-set-col");
    col = col.child(restart_note(app));
    for agent in AGENT_PREFERENCE {
        col = col.child(agent_section(app, agent));
    }
    col
}

/// "Applies on restart", always on the page; it says "saved" once something
/// has changed, with a one-click restart.
fn restart_note(app: &App) -> impl ElementBuilder {
    let changed = app.bindings.restart_note.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-set-restart-note")
        .deps([changed.signal_id()])
        .on_state(move |_| {
            let (text, class) = if changed.get() {
                (
                    tr(
                        &app,
                        "Saved. Restart Muqun for agent changes to take effect.",
                        "已保存。重启 Muqun 后，agent 的改动才会生效。",
                    ),
                    "mq-note--changed",
                )
            } else {
                (
                    tr(
                        &app,
                        "Agent changes are saved at once and take effect the next time Muqun starts.",
                        "agent 的改动会立即保存，下次启动 Muqun 时生效。",
                    ),
                    "mq-note",
                )
            };
            let mut note = div()
                .class(class)
                .w_full()
                .flex_row()
                .items_center()
                .child(div().flex_1().child(ui_text(text).size(12.0)));
            if changed.get() {
                note = note.child(
                    button(app.strings.restart_now())
                        .variant(ButtonVariant::Outline)
                        .size(ButtonSize::Small)
                        .on_click(move |_| {
                            crate::platform::shell::relaunch_self();
                            blinc_layout::window_actions::close_window();
                        }),
                );
            }
            note
        })
}

/// Sign-in wording, as the gateway's `agent setup` prints it.
fn agent_hint(app: &App, agent: &str) -> Option<&'static str> {
    Some(match agent {
        "codex" => tr(
            app,
            "Install Codex 0.163.0 or newer and sign in on this host yourself.",
            "请自行安装 Codex 0.163.0 或更新版本，并在这台电脑上登录。",
        ),
        "claude" => tr(
            app,
            "Sign in to Claude Code on this host yourself (run claude once).",
            "请自行在这台电脑上登录 Claude Code（运行一次 claude）。",
        ),
        "qoder" => tr(
            app,
            "Configure and sign in to Qoder CLI on this host yourself.",
            "请自行在这台电脑上配置并登录 Qoder CLI。",
        ),
        "fx" => tr(
            app,
            "Sign in and configure FX on this host yourself.",
            "请自行在这台电脑上登录并配置 FX。",
        ),
        "pi" => tr(
            app,
            "Configure providers and sign in to Pi on this host yourself.",
            "请自行在这台电脑上配置 Pi 的模型提供方并登录。",
        ),
        "antigravity" => tr(
            app,
            "Install Antigravity's CLI and sign in with your Google account by running `agy` once in a terminal on this host.",
            "请安装 Antigravity 的命令行，并在这台电脑的终端里运行一次 `agy`，用 Google 账号登录。",
        ),
        "cursor" => tr(
            app,
            "Sign in once with `agent login` in a terminal on this host.",
            "请在这台电脑的终端里运行一次 `agent login` 登录。",
        ),
        "deepseek" => tr(
            app,
            "Muqun only joins a DeepSeek harness that is already running; it never starts one.",
            "Muqun 只会接入已经在运行的 DeepSeek harness，不会自己启动。",
        ),
        _ => return None,
    })
}

fn agent_section(app: &App, agent: &'static str) -> Div {
    let ctx = BlincContextState::get();
    let config = app.config.get().agents;
    let infos = app.bindings.agents.get();
    let standing = infos
        .iter()
        .find(|i| i.id == agent)
        .map(Standing::of)
        .unwrap_or(Standing::Checking);

    let enabled = ctx.use_state_keyed(&format!("mq-set.{agent}.enabled"), || config.enabled(agent));
    let toggle = {
        let app = app.clone();
        switch(&enabled).on_change(move |on| {
            app.send(Command::Edit(ConfigEdit::AgentEnabled {
                agent: agent.to_string(),
                on,
            }));
            app.bindings.restart_note.signal().set(true);
            if agent == "t3" && !on {
                // `set_enabled` dropped the URL too; the field follows.
                clear_text(&format!("mq-set.t3.{}", Field::Url.key()));
            }
        })
    };

    let mut section = div().class("mq-agent-section").w_full().flex_col().child(
        div()
            .w_full()
            .flex_row()
            .items_center()
            .class("mq-agent-section-head")
            .child(agents::dot(agent))
            .child(
                div().class("mq-agent-section-name").flex_1().child(
                    ui_text(agents::name(agent, &infos))
                        .size(14.0)
                        .semibold()
                        .no_wrap(),
                ),
            )
            .child(
                div()
                    .class("mq-standing")
                    .class(standing.class())
                    .child(ui_text(standing.word(&app.strings)).size(11.0).no_wrap()),
            )
            .child(toggle),
    );
    if let Some(text) = agent_hint(app, agent) {
        section = section.child(hint(text));
    }
    for field in agent_fields::fields(agent) {
        section = section.child(field_row(app, agent, *field, &config));
    }
    if agent == "t3" {
        section = section.child(pair_t3(app, &config));
    }
    section
}

fn clear_text(key: &str) {
    let data = text_data(key, "");
    if let Ok(mut d) = data.lock() {
        d.value.clear();
        d.cursor = 0;
    };
}

fn field_label(app: &App, field: Field) -> &'static str {
    match field {
        Field::Binary => tr(app, "Executable", "可执行文件"),
        Field::Autostart => tr(app, "Start it when needed", "需要时自动启动"),
        Field::Url => tr(app, "Server address", "服务器地址"),
        Field::Endpoint => tr(app, "Harness address", "Harness 地址"),
        Field::RuntimeMode => tr(app, "Mode for new threads", "新线程的运行模式"),
        Field::AcpPackage => tr(
            app,
            "ACP package (to pin a version)",
            "ACP 包（用于固定版本）",
        ),
        Field::SessionDir => tr(app, "Session folder", "会话文件夹"),
        Field::Approvals => tr(app, "Tool approvals", "工具审批"),
        Field::IdleTimeout => tr(
            app,
            "Close an idle helper after (seconds)",
            "空闲多少秒后关闭辅助进程",
        ),
        Field::PairingToken => tr(app, "Pairing token", "配对令牌"),
    }
}

fn field_hint(app: &App, agent: &str, field: Field) -> Option<&'static str> {
    Some(match (agent, field) {
        ("opencode", Field::Autostart) => tr(
            app,
            "Ignored while a Muqun gateway is running on this computer: then OpenCode is only joined, never started.",
            "这台电脑上有 Muqun gateway 在运行时此项无效：只接入 OpenCode，不会启动它。",
        ),
        ("pi" | "opencode", Field::Binary) => tr(
            app,
            "A path or a program name; empty finds it on PATH.",
            "路径或程序名；留空则在 PATH 中查找。",
        ),
        (_, Field::Binary) => tr(
            app,
            "An absolute path; empty finds it on PATH.",
            "绝对路径；留空则在 PATH 中查找。",
        ),
        ("t3", Field::Url) => tr(
            app,
            "Setting an address also turns T3 on. Empty uses http://127.0.0.1:3773 when T3 is on.",
            "填写地址会同时启用 T3；留空且已启用时使用 http://127.0.0.1:3773。",
        ),
        ("pi", Field::Approvals) => tr(
            app,
            "ask: bash, edit and write wait for your approval. never: Pi runs them on its own.",
            "ask：bash、edit、write 需要你批准；never：Pi 自行执行。",
        ),
        _ => return None,
    })
}

fn field_row(app: &App, agent: &'static str, field: Field, config: &AgentConfigs) -> Div {
    let ctx = BlincContextState::get();
    let key = format!("mq-set.{agent}.{}", field.key());
    let label = field_label(app, field);
    let initial = config.value(agent, field);
    let mut row = div().class("mq-set-field").w_full().flex_col();
    let send = {
        let app = app.clone();
        move |value: String| {
            app.send(Command::Edit(ConfigEdit::AgentValue {
                agent: agent.to_string(),
                field,
                value,
            }));
            app.bindings.restart_note.signal().set(true);
        }
    };
    match field.kind() {
        Kind::Flag => {
            let state = ctx.use_state_keyed(&key, || initial == "true");
            row = row.child(
                switch(&state)
                    .label(label)
                    .on_change(move |on| send(on.to_string())),
            );
        }
        Kind::Choice(options) => {
            let state = ctx.use_state_keyed(&key, || initial.clone());
            let mut select = SelectBuilder::with_key(format!("{key}.select"), &state)
                .label(label)
                .w(260.0);
            for option in options {
                select = select.option(*option, *option);
            }
            row = row.child(select.on_change(move |value| send(value.to_string())));
        }
        Kind::Text | Kind::Number => {
            let data = text_data(&key, &initial);
            let error = ctx.use_state_keyed(&format!("{key}.err"), String::new);
            let shown = error.clone();
            row = row
                .child(
                    text_field(&data)
                        .label(label)
                        .w_full()
                        .on_change(move |text| {
                            match AgentConfigs::check(agent, field, text) {
                                Ok(()) => {
                                    error.set(String::new());
                                    send(text.to_string());
                                }
                                // Not saved; the line below says why.
                                Err(why) => error.set(why.to_string()),
                            }
                        }),
                )
                .child(error_line(format!("{key}.err-line"), shown));
        }
    }
    if let Some(text) = field_hint(app, agent, field) {
        row = row.child(hint(text));
    }
    row
}

/// T3 needs pairing: the token `t3 pair` prints goes into the config, and
/// the runtime exchanges it for a bearer when the desktop next starts.
fn pair_t3(app: &App, config: &AgentConfigs) -> Div {
    let ctx = BlincContextState::get();
    let key = format!("mq-set.t3.{}", Field::PairingToken.key());
    let data = text_data(&key, "");
    let waiting = !config.value("t3", Field::PairingToken).is_empty();
    let saved = ctx.use_state_keyed(&format!("{key}.saved"), move || waiting);
    let pair = {
        let app = app.clone();
        let data = data.clone();
        let saved = saved.clone();
        button(tr(&app, "Pair with T3", "与 T3 配对"))
            .size(ButtonSize::Small)
            .on_click(move |_| {
                let token = data
                    .lock()
                    .map(|d| d.value.trim().to_string())
                    .unwrap_or_default();
                if token.is_empty() {
                    return;
                }
                app.send(Command::Edit(ConfigEdit::AgentValue {
                    agent: "t3".into(),
                    field: Field::PairingToken,
                    value: token,
                }));
                app.send(Command::Edit(ConfigEdit::AgentEnabled {
                    agent: "t3".into(),
                    on: true,
                }));
                app.bindings.restart_note.signal().set(true);
                if let Ok(mut d) = data.lock() {
                    d.value.clear();
                    d.cursor = 0;
                }
                saved.set(true);
            })
    };
    let note = {
        let app = app.clone();
        stateful_with_key::<NoState>("mq-set-t3-saved")
            .deps([saved.signal_id()])
            .on_state(move |_| {
                if !saved.get() {
                    return div();
                }
                div().class("mq-set-hint").w_full().child(
                    ui_text(tr(
                        &app,
                        "Pairing token saved. It is exchanged once when Muqun starts; restart to finish pairing.",
                        "配对令牌已保存。它会在 Muqun 启动时兑换一次；请重启以完成配对。",
                    ))
                    .size(12.0),
                )
            })
    };
    div()
        .class("mq-set-field")
        .w_full()
        .flex_col()
        .child(
            text_field(&data)
                .label(tr(app, "Pair with T3 — token from `t3 pair`", "与 T3 配对 — `t3 pair` 给出的令牌"))
                .placeholder(tr(app, "Paste the one-time token", "粘贴一次性令牌"))
                .w_full(),
        )
        .child(div().class("mq-set-actions").flex_row().child(pair))
        .child(note)
        .child(hint(tr(
            app,
            "Run `t3 pair` on the T3 host. Muqun keeps its own T3 credential, separate from the gateway's.",
            "在 T3 所在的电脑上运行 `t3 pair`。Muqun 保存自己的 T3 凭据，与 gateway 的互不相干。",
        )))
}

// --- shared choice row (General language, …) ------------------------------

/// A row of buttons, one per choice, the chosen one filled. `current` holds
/// the chosen value; `pick` runs when another is clicked.
fn choice_row<T: Copy + PartialEq + Default + Send + Sync + 'static>(
    key: &str,
    current: State<T>,
    choices: Vec<(T, &'static str)>,
    pick: impl Fn(T) + Clone + Send + Sync + 'static,
) -> impl ElementBuilder {
    stateful_with_key::<NoState>(key.to_string())
        .deps([current.signal_id()])
        .on_state(move |_| {
            let chosen = current.get();
            let mut row = div().class("mq-set-choices").flex_row();
            for (value, label) in choices.clone() {
                let (current, pick) = (current.clone(), pick.clone());
                row = row.child(
                    button(label)
                        .variant(if value == chosen {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Outline
                        })
                        .size(ButtonSize::Small)
                        .on_click(move |_| {
                            if current.get() != value {
                                current.signal().set(value);
                                pick(value);
                            }
                        }),
                );
            }
            row
        })
}

// --- Appearance -----------------------------------------------------------

/// Density / motion / glass above the themes library (own scroll).
fn appearance_tab(app: &App, width: f32, height: f32) -> Div {
    let ctx = BlincContextState::get();
    let appearance = app.config.get().appearance;

    let density = ctx.use_state_keyed("mq-set.density", {
        let initial = appearance.density;
        move || initial
    });
    let density_app = app.clone();
    let density_row = choice_row(
        "mq-set-density",
        density,
        vec![
            (Density::Comfortable, tr(app, "Comfortable", "宽松")),
            (Density::Compact, tr(app, "Compact", "紧凑")),
        ],
        move |density| density_app.apply_density(density),
    );

    let motion = ctx.use_state_keyed("mq-set.reduced_motion", || appearance.reduced_motion);
    let motion_app = app.clone();
    let motion_switch = switch(&motion)
        .label(tr(app, "Reduce motion", "减少动效"))
        .on_change(move |on| motion_app.apply_reduced_motion(on));

    let glass = ctx.use_state_keyed("mq-set.reduced_transparency", || {
        appearance.reduced_transparency
    });
    let glass_app = app.clone();
    let glass_switch = switch(&glass)
        .label(tr(app, "Reduce transparency", "减少透明"))
        .on_change(move |on| glass_app.apply_reduced_transparency(on));

    // Reading prefs sit above the library; leave the rest for themes_panel.
    const READING: f32 = 168.0;
    let themes_h = (height - READING).max(160.0);

    div()
        .w_full()
        .h(height)
        .flex_col()
        .child(
            div()
                .class("mq-set-col")
                .class("mq-appearance-reading")
                .w_full()
                .flex_col()
                .child(section_title(tr(app, "Reading", "阅读")))
                .child(
                    div()
                        .class("mq-set-field")
                        .w_full()
                        .flex_col()
                        .child(ui_text(tr(app, "Density", "密度")).size(12.0).semibold())
                        .child(density_row),
                )
                .child(motion_switch)
                .child(glass_switch)
                .child(section_title(tr(app, "Themes", "主题"))),
        )
        .child(super::themes::themes_panel(app, width, themes_h))
}

// --- General --------------------------------------------------------------

fn general_tab(app: &App) -> Div {
    let ctx = BlincContextState::get();
    let general = app.config.get().general;

    let language = ctx.use_state_keyed("mq-set.language", {
        let initial = app.strings.language();
        move || initial
    });
    let apply = app.clone();
    let language_row = choice_row(
        "mq-set-language",
        language,
        vec![(Language::ZhCn, "简体中文"), (Language::En, "English")],
        move |language| apply.apply_language(language),
    );

    let cwd_data = text_data(
        "mq-set.default_cwd",
        &general
            .default_cwd
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let cwd_error = ctx.use_state_keyed("mq-set.default_cwd.err", String::new);
    let cwd_shown = cwd_error.clone();
    let cwd_app = app.clone();
    let cwd_field = text_field(&cwd_data)
        .label(tr(
            app,
            "Default folder for new sessions",
            "新会话的默认文件夹",
        ))
        .placeholder("~")
        .w_full()
        .on_change(move |text| {
            let text = text.trim();
            if text.is_empty() {
                cwd_error.set(String::new());
                cwd_app.send(Command::Edit(ConfigEdit::DefaultCwd(None)));
                return;
            }
            match muqun_gateway::agents::directories::expand_home(text).filter(|p| p.is_absolute())
            {
                Some(path) => {
                    cwd_error.set(String::new());
                    cwd_app.send(Command::Edit(ConfigEdit::DefaultCwd(Some(path))));
                }
                None => cwd_error.set(
                    tr(
                        &cwd_app,
                        "Use an absolute path or ~/…",
                        "请输入绝对路径或 ~/…",
                    )
                    .into(),
                ),
            }
        });

    let editor_data = text_data(
        "mq-set.editor",
        general.editor_command.as_deref().unwrap_or_default(),
    );
    let editor_app = app.clone();
    let editor_field = text_field(&editor_data)
        .label(tr(app, "Editor command", "编辑器命令"))
        .placeholder("code, zed, …")
        .w_full()
        .on_change(move |text| {
            let command = Some(text.trim().to_string()).filter(|c| !c.is_empty());
            editor_app.send(Command::Edit(ConfigEdit::EditorCommand(command)));
        });

    let tray = ctx.use_state_keyed("mq-set.close_to_tray", || false);

    div()
        .w_full()
        .flex_col()
        .class("mq-set-col")
        .child(section_title(tr(app, "Language", "语言")))
        .child(language_row)
        .child(section_title(tr(app, "Sessions", "会话")))
        .child(
            div()
                .class("mq-set-field")
                .w_full()
                .flex_col()
                .child(cwd_field)
                .child(error_line("mq-set.default_cwd.err-line".into(), cwd_shown)),
        )
        .child(section_title(tr(app, "Editor", "编辑器")))
        .child(
            div()
                .class("mq-set-field")
                .w_full()
                .flex_col()
                .child(editor_field),
        )
        .child(hint(tr(
            app,
            "Used by “Open in editor”, which arrives in a later version.",
            "供之后的“在编辑器中打开”使用，该功能将在后续版本提供。",
        )))
        .child(section_title(tr(app, "Window", "窗口")))
        .child(
            switch(&tray)
                .label(tr(app, "Close to tray", "关闭窗口时收起到托盘"))
                .disabled(true),
        )
        .child(hint(tr(
            app,
            "Not available yet: closing the window quits the app.",
            "暂不可用：目前关闭窗口即退出应用。",
        )))
}

// --- About ----------------------------------------------------------------

fn about_tab(app: &App) -> Div {
    let config_file = app.config.path().to_path_buf();
    let config_dir = app.paths.config_dir.clone();
    let open = button(tr(app, "Open config folder", "打开配置文件夹"))
        .variant(ButtonVariant::Outline)
        .size(ButtonSize::Small)
        .on_click(move |_| crate::platform::shell::open_folder(&config_dir));
    let path_row = |label: &'static str, path: String| {
        div()
            .class("mq-set-field")
            .w_full()
            .flex_col()
            .child(ui_text(label).size(12.0).semibold())
            .child(mono_text(path).size(12.0))
    };
    div()
        .w_full()
        .flex_col()
        .class("mq-set-col")
        .child(
            ui_text(format!(
                "{} {}",
                crate::branding::APP_NAME,
                env!("CARGO_PKG_VERSION")
            ))
            .size(16.0)
            .semibold(),
        )
        .child(hint(tr(
            app,
            "Muqun keeps its own files, apart from a Muqun gateway's.",
            "Muqun 使用自己的文件夹，与 Muqun gateway 的互不相干。",
        )))
        .child(path_row(
            tr(app, "Config file", "配置文件"),
            config_file.to_string_lossy().into_owned(),
        ))
        .child(path_row(
            tr(app, "State folder", "状态文件夹"),
            app.paths.state_dir.to_string_lossy().into_owned(),
        ))
        .child(div().class("mq-set-actions").flex_row().child(open))
}
