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
    let t = |en| tr_ref(language, en);
    match field {
        Field::Binary => (
            t("Executable"),
            t("A command or path. Empty looks it up on PATH."),
        ),
        Field::Autostart => (
            t("Start with Muqun"),
            t("Launch its server when Muqun starts."),
        ),
        Field::Url => (t("Server address"), t("Where its server listens.")),
        Field::Endpoint => (t("Endpoint"), t("The harness's HTTP address.")),
        Field::RuntimeMode => (t("Runtime mode"), t("How much it may do without asking.")),
        Field::AcpPackage => (
            t("ACP package"),
            t("The npm package that speaks ACP for it."),
        ),
        Field::SessionDir => (t("Session folder"), t("Where it keeps its sessions.")),
        Field::Approvals => (t("Approvals"), t("Ask before tools run, or never ask.")),
        Field::IdleTimeout => (
            t("Idle timeout"),
            t("Seconds before an idle agent process stops."),
        ),
        Field::PairingToken => (t("Pairing token"), t("From `t3 pair`; used once.")),
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
    let stored = s.cfg.agents_for(s.current_computer()).value(agent, field);
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
                    let stored = s.cfg.agents_for(s.current_computer()).value(&agent_in, f);
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
    let enabled = s.cfg.agents_for(s.current_computer()).enabled(id);
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
            tr_ref(language, "Enabled"),
            move |s| {
                s.edit(ConfigEdit::AgentEnabled {
                    agent: toggle_id.clone(),
                    on: !enabled,
                });
                s.send(Command::Refresh);
            },
        ))
}

/// Totals are snapshots from loaded sessions, never increments from repeated events.
#[derive(Default)]
struct Usage {
    sessions: usize,
    reported: usize,
    input: u64,
    output: u64,
    cost: Option<f64>,
    cost_sessions: usize,
}

fn usage<'a>(
    sessions: impl Iterator<Item = &'a muqun_gateway::agents::AgentSessionInfo>,
    agent: &str,
) -> Usage {
    let mut total = Usage::default();
    let mut seen = std::collections::HashSet::new();
    for session in sessions.filter(|session| session.agent_id == agent) {
        if !seen.insert(&session.asid) {
            continue;
        }
        total.sessions += 1;
        if let Some(tokens) = &session.tokens {
            total.reported += 1;
            total.input = total.input.saturating_add(tokens.input);
            total.output = total.output.saturating_add(tokens.output);
        }
        if let Some(cost) = session.cost.filter(|cost| cost.is_finite() && *cost >= 0.0) {
            total.cost = Some(total.cost.unwrap_or(0.0) + cost);
            total.cost_sessions += 1;
        }
    }
    total
}

fn usage_card(s: &UiState, k: &Kit, agent: &str) -> El {
    let t = |en| tr_ref(s.cfg.general.language, en);
    let total = usage(s.sessions.iter(), agent);
    let missing = t("Not reported");
    let values = [
        ("Sessions", total.sessions.to_string()),
        (
            "Input tokens",
            if total.reported > 0 {
                total.input.to_string()
            } else {
                missing.to_string()
            },
        ),
        (
            "Output tokens",
            if total.reported > 0 {
                total.output.to_string()
            } else {
                missing.to_string()
            },
        ),
        (
            "Estimated cost",
            total
                .cost
                .map(|cost| format!("${cost:.4}"))
                .unwrap_or_else(|| missing.to_string()),
        ),
    ];
    let mut cards = hrow().key("usage-metrics").w_full().gap(8.0).flex_wrap();
    for (label, value) in values {
        cards = cards.child(
            col()
                .key(label)
                .min_w(120.0)
                .grow(1.0)
                .px(12.0)
                .py(12.0)
                .gap(6.0)
                .radius(k.radius.sm)
                .fill(k.c.bg_accent.alpha(0.06))
                .child(text(t(label)).font(k.ty.small).color(k.c.text_muted))
                .child(text(value).font(k.ty.mono).color(k.c.text_base)),
        );
    }
    col().key("agent-usage").w_full().gap(8.0).py(12.0)
        .child(cards)
        .child(text(t("Loaded sessions in this view only; not account quota. Input/output exclude separately reported cache and reasoning tokens."))
            .font(k.ty.small).color(k.c.text_muted).wrap().w_full())
        .child(text(format!("{}: {}/{} · {}: {}/{}", t("Token coverage"), total.reported, total.sessions,
            t("Cost coverage"), total.cost_sessions, total.sessions)).font(k.ty.small).color(k.c.text_faint))
}

pub(super) fn actions(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    vec![super::button(tr_ref(language, "Look again"))
        .icon(&crate::ui::icons::REFRESH)
        .help(tr_ref(language, "Probe this computer for agents again"))
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
            tr_ref(language, "Looking for agents…"),
            tr_ref(
                language,
                "Muqun probes the agents installed on this computer.",
            ),
        )];
    }

    let mut elements: Vec<El> = Vec::new();

    let agent_groups =
        crate::ui::agents::by_standing(&s.agents)
            .into_iter()
            .map(|(a, standing)| {
                let mut rows: Vec<El> = vec![usage_card(s, k, &a.id)];
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
                let enabled = s.cfg.agents_for(s.current_computer()).enabled(&a.id);
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
            });

    elements.extend(agent_groups);
    elements
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_counts_reported_snapshots_once_and_preserves_unknowns() {
        let reported: muqun_gateway::agents::AgentSessionInfo =
            serde_json::from_value(serde_json::json!({
                "asid":"a", "agent_id":"codex", "title":"one", "status":"idle", "updated_ms":0,
                "tokens":{"input":100,"output":20,"cache_read":300}, "cost":0.25
            }))
            .unwrap();
        let mut unknown = reported.clone();
        unknown.asid.0 = "b".into();
        unknown.tokens = None;
        unknown.cost = None;
        let mut other = reported.clone();
        other.asid.0 = "c".into();
        other.agent_id = "claude".into();
        let total = usage(
            [&reported, &reported, &unknown, &other].into_iter(),
            "codex",
        );
        assert_eq!(
            (total.sessions, total.reported, total.input, total.output),
            (2, 1, 100, 20)
        );
        assert_eq!(total.cost, Some(0.25));
        assert_eq!(total.cost_sessions, 1);
        let missing = usage([&unknown].into_iter(), "codex");
        assert_eq!(missing.cost, None);
        assert_eq!(missing.reported, 0);
    }
}
