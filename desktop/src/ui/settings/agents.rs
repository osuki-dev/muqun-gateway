//! Settings → Agents: each agent found on this computer as a group, its
//! switch in the group's header and its own fields as rows.

use rune::{col, el, row as hrow, text, El, TextEdit, Tx};
use rune_kit::{Badge, BadgeVariant, Kit, TextInput};

use super::{choice, edit_of, empty, flag, group_with, row, select, status, Tone};
use crate::agent_fields::{fields, Field, Kind};
use crate::config::{AgentConfigs, ConfigEdit, Language};
use crate::model::commands::Command;
use crate::ui::agents::Standing;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

/// What a field is called and what it is for.
fn words(language: Language, field: Field) -> (&'static str, &'static str) {
    let t = |en, zh| tr_ref(language, en, zh);
    match field {
        Field::Binary => (
            t("Executable", "可执行文件"),
            t(
                "A command or path. Empty looks it up on PATH.",
                "命令或路径。留空则在 PATH 中查找。",
            ),
        ),
        Field::Autostart => (
            t("Start with Muqun", "随 Muqun 启动"),
            t(
                "Launch its server when Muqun starts.",
                "Muqun 启动时一并启动它的服务。",
            ),
        ),
        Field::Url => (
            t("Server address", "服务地址"),
            t("Where its server listens.", "它的服务监听的地址。"),
        ),
        Field::Endpoint => (
            t("Endpoint", "接口地址"),
            t("The harness's HTTP address.", "Harness 的 HTTP 地址。"),
        ),
        Field::RuntimeMode => (
            t("Runtime mode", "运行模式"),
            t(
                "How much it may do without asking.",
                "无需询问即可执行的范围。",
            ),
        ),
        Field::AcpPackage => (
            t("ACP package", "ACP 包"),
            t(
                "The npm package that speaks ACP for it.",
                "为它提供 ACP 的 npm 包。",
            ),
        ),
        Field::SessionDir => (
            t("Session folder", "会话目录"),
            t("Where it keeps its sessions.", "它保存会话的目录。"),
        ),
        Field::Approvals => (
            t("Approvals", "审批"),
            t(
                "Ask before tools run, or never ask.",
                "工具运行前询问，或从不询问。",
            ),
        ),
        Field::IdleTimeout => (
            t("Idle timeout", "空闲超时"),
            t(
                "Seconds before an idle agent process stops.",
                "Agent 进程空闲多少秒后停止。",
            ),
        ),
        Field::PairingToken => (
            t("Pairing token", "配对令牌"),
            t("From `t3 pair`; used once.", "来自 `t3 pair`，仅使用一次。"),
        ),
    }
}

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

fn agent_field(s: &UiState, k: &Kit, layer: &mut Vec<El>, agent: &str, field: Field) -> El {
    let language = s.cfg.general.language;
    let slot = format!("{agent}.{}", field.key());
    let stored = s.cfg.agents.value(agent, field);
    let (title, detail) = words(language, field);
    let (a, f) = (agent.to_string(), field);
    let control = match field.kind() {
        Kind::Flag => {
            let on = stored == "true";
            flag(k, &slot, on, title, move |s| {
                set_field(s, &a, f, (!on).to_string())
            })
        }
        Kind::Choice(options) => {
            let at = options.iter().position(|o| *o == stored).unwrap_or(0);
            let pick = move |s: &mut UiState, i: usize| set_field(s, &a, f, options[i].to_string());
            if options.len() > 3 {
                select(s, k, layer, &slot, options, at, pick)
            } else {
                choice(k, &slot, options, at, pick)
            }
        }
        Kind::Text | Kind::Number => {
            let edit = edit_of(s, &slot, || stored.clone());
            let (key_in, key_ch, agent_in) = (slot.clone(), slot.clone(), agent.to_string());
            let input = TextInput::new(&edit)
                .label(title)
                .placeholder(match field {
                    Field::Binary => agent.to_string(),
                    Field::IdleTimeout => "600".to_string(),
                    _ => String::new(),
                })
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
                });
            let input = super::field(input, &edit, k, &slot);
            match s.agent_errors.get(&slot) {
                Some(err) => col()
                    .key(format!("field-{slot}"))
                    .w(super::control_w())
                    .gap(6.0)
                    .child(input)
                    .child(status(k, "error", Tone::Bad, err.clone())),
                None => input,
            }
        }
    };
    row(k, &slot, title, detail, control)
}

/// The agent's header: its mark, name, version and standing, and its
/// switch at the trailing edge (where every row keeps its control).
fn head(s: &UiState, k: &Kit, id: &str, standing: Standing, version: Option<&str>) -> El {
    let language = s.cfg.general.language;
    let enabled = s.cfg.agents.enabled(id);
    let brand = crate::ui::agents::color(id, s.dark);
    let name = crate::ui::agents::name(id, &s.agents);
    let variant = match standing {
        Standing::Usable => BadgeVariant::Accent,
        Standing::Checking => BadgeVariant::Subtle,
        _ => BadgeVariant::Neutral,
    };
    let mark = el()
        .key("mark")
        .size(32.0, 32.0)
        .radius(9.0)
        .fill(brand.alpha(if enabled { 0.16 } else { 0.08 }))
        .center()
        .child(crate::ui::agents::mark(id, 18.0, s.dark))
        .a11y_hidden();
    let mut meta = hrow().key("meta").gap(8.0).center_y().child(
        Badge::new(standing.word(language).to_string())
            .variant(variant)
            .build(k),
    );
    if let Some(v) = version {
        meta = meta.child(text(format!("v{v}")).font(k.ty.small).color(k.c.text_faint));
    }
    let toggle_id = id.to_string();
    hrow()
        .key("head")
        .w_full()
        .gap(12.0)
        .center_y()
        .pb(4.0)
        .child(mark)
        .child(
            col()
                .key("words")
                .grow(1.0)
                .shrink()
                .gap(2.0)
                .child(text(name).font(k.ty.ui_medium).color(if enabled {
                    k.c.text_base
                } else {
                    k.c.text_muted
                }))
                .child(meta),
        )
        .child(flag(
            k,
            &format!("on-{id}"),
            enabled,
            tr_ref(language, "Enabled", "启用"),
            move |s| {
                s.edit(ConfigEdit::AgentEnabled {
                    agent: toggle_id.clone(),
                    on: !enabled,
                });
                s.send(Command::Refresh);
            },
        ))
}

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    vec![super::button(tr_ref(language, "Look again", "重新发现"))
        .icon(&crate::ui::icons::REFRESH)
        .help(tr_ref(
            language,
            "Probe this computer for agents again",
            "重新探测这台电脑上的 Agents",
        ))
        .on_press(|s: &mut UiState| s.send(Command::Refresh))
        .build(k)
        .key("agents-refresh")]
}

pub(super) fn page(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> Vec<El> {
    let language = s.cfg.general.language;
    if s.agents.is_empty() {
        return vec![empty(
            k,
            "agents-empty",
            tr_ref(language, "Looking for agents…", "正在发现 Agents…"),
            tr_ref(
                language,
                "Muqun probes the agents installed on this computer.",
                "Muqun 会探测这台电脑上已安装的 Agents。",
            ),
        )];
    }
    crate::ui::agents::by_standing(&s.agents)
        .into_iter()
        .map(|(a, standing)| {
            let mut rows: Vec<El> = Vec::new();
            if let Some(reason) = &a.reason {
                rows.push(el().key("reason").w_full().py(10.0).child(status(
                    k,
                    "reason",
                    Tone::Quiet,
                    reason.clone(),
                )));
            }
            for f in fields(&a.id) {
                rows.push(agent_field(s, k, layer, &a.id, *f));
            }
            let enabled = s.cfg.agents.enabled(&a.id);
            let group = group_with(
                k,
                &format!("agent-{}", a.id),
                head(s, k, &a.id, standing, a.version.as_deref()),
                rows,
            );
            if enabled {
                group
            } else {
                group.opacity(0.72)
            }
        })
        .collect()
}
