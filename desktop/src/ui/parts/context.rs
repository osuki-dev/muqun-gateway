//! How full a session's context is, and the agent's account limits (App
//! `agent-context-sheet` / `agent-account-limits-section`): a compact meter
//! for the session header and the sheet it opens. Only data the gateway
//! already gives: `context_usage` on the session, and
//! `GET /api/agents/{id}/account-limits` where the agent offers it.

use std::collections::HashSet;

use muqun_gateway::agents::{
    AgentPart, AgentSessionId, AgentSessionInfo, AgentSessionStatus, CompactionStatus,
};
use rune::{col, row, text, Color, Cursor, El};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit};
use serde_json::Value;

use super::glyphs;
use crate::config::Language;
use crate::model::commands::Command;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

/// `175k`, `1.2M`.
pub fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n if n >= 1000 => format!("{:.1}k", n as f64 / 1000.0),
        n => n.to_string(),
    }
}

pub fn tokens_label(_language: Language, n: u64) -> String {
    format!("{} tokens", compact(n))
}

fn limit_from_value(v: &Value) -> Option<u64> {
    v.get("context")
        .or(Some(v))
        .and_then(Value::as_u64)
        .filter(|l| *l > 0)
}

/// Resolve the model's context window: session `context_usage.limit` first,
/// then catalog model `limit.context`, then `info.limit.context`.
fn resolved_limit(s: &UiState, info: &AgentSessionInfo) -> Option<u64> {
    info.context_usage
        .as_ref()
        .and_then(|u| u.limit.as_ref())
        .and_then(limit_from_value)
        .or_else(|| {
            let model = info.model.as_ref()?;
            let catalog = s.catalog_for_session(&info.asid.0)?;
            crate::ui::labels::catalog_model_info(catalog, model)
                .and_then(|m| m.limit.as_ref())
                .and_then(limit_from_value)
        })
        .or_else(|| info.limit.as_ref().and_then(limit_from_value))
}

/// What the model holds now and the most it can: (used, limit).
fn usage(s: &UiState, info: &AgentSessionInfo) -> Option<(u64, Option<u64>)> {
    let usage = info.context_usage.as_ref()?;
    let limit = resolved_limit(s, info);
    (usage.total_tokens > 0).then_some((usage.total_tokens, limit))
}

/// Count messages currently in context: explicit `context_usage.limit.messages`
/// when reported by the backend, otherwise distinct messages in the loaded
/// timeline after the latest completed compaction.
fn context_message_count(s: &UiState, asid: &str, info: &AgentSessionInfo) -> Option<usize> {
    if let Some(n) = info
        .context_usage
        .as_ref()
        .and_then(|u| u.limit.as_ref())
        .and_then(|l| l.get("messages"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        return Some(n as usize);
    }
    let tl = s.timelines.get(asid)?;
    let start = tl
        .items
        .iter()
        .rposition(|it| {
            matches!(
                it.part,
                AgentPart::Compaction {
                    status: CompactionStatus::Completed,
                    ..
                }
            )
        })
        .map_or(0, |idx| idx + 1);
    let mut ids = HashSet::new();
    for item in &tl.items[start..] {
        if !matches!(item.part, AgentPart::Compaction { .. }) && !item.message_id.is_empty() {
            ids.insert(item.message_id.as_str());
        }
    }
    (!ids.is_empty()).then_some(ids.len())
}

fn is_compacting(s: &UiState, asid: &str) -> bool {
    if s.compacting_sessions.contains(asid) {
        return true;
    }
    s.timelines.get(asid).is_some_and(|tl| {
        tl.items
            .iter()
            .rev()
            .find_map(|it| match &it.part {
                AgentPart::Compaction { status, .. } => Some(*status),
                _ => None,
            })
            .is_some_and(|st| matches!(st, CompactionStatus::Started | CompactionStatus::Running))
    })
}

fn tone(k: &Kit, fraction: f32) -> Color {
    if fraction >= 0.95 {
        k.c.danger.icon
    } else if fraction >= 0.85 {
        k.c.warning.icon
    } else {
        k.c.icon_accent
    }
}

/// A lit leading edge and a darker tail; only geometry changes animate.
fn progress_fill(k: &Kit, color: Color, height: f32) -> El {
    let tip = color.mix(k.c.white, 0.38);
    col()
        .key("progress-fill")
        .h(height)
        .radius(height / 2.0)
        .linear_gradient(
            1.57,
            [
                (0.0, color.mix(k.c.bg_base, 0.35)),
                (0.65, color),
                (1.0, tip),
            ],
        )
        .shadow(0.0, height * 1.8, color.alpha(0.32))
        .token_motion(k.motion.disclosure)
        .child(
            col()
                .pin(Some(1.0), Some(0.0), Some(1.0), None)
                .h(1.0)
                .radius(0.5)
                .fill(k.c.white.alpha(0.28)),
        )
        .child(
            col()
                .pin(None, Some(0.0), Some(0.0), None)
                .size(1.0, height)
                .radius(0.5)
                .fill(tip)
                .shadow(0.0, height, color.alpha(0.5)),
        )
}

fn bar(k: &Kit, key: &str, fraction: f32, width: f32, height: f32) -> El {
    let fraction = fraction.clamp(0.0, 1.0);
    row()
        .key(key.to_string())
        .w(width)
        .h(height)
        .radius(height / 2.0)
        .fill(k.alpha(k.c.text_base, 0.09))
        .children(
            (fraction > 0.0)
                .then(|| progress_fill(k, tone(k, fraction), height).w(width * fraction)),
        )
}

fn supports_limits(s: &UiState, agent: &str) -> bool {
    s.agents
        .iter()
        .any(|a| a.id == agent && a.features.extra.get("accountLimits") == Some(&Value::Bool(true)))
}

/// Open the sheet for `asid`, asking for its agent's limits once.
pub fn open(s: &mut UiState, asid: &str) {
    if s.context_open.as_deref() == Some(asid) {
        s.context_open = None;
        return;
    }
    s.menu = None;
    s.context_open = Some(asid.to_string());
    if let Some(agent) = s.session_by_id(asid).map(|i| i.agent_id.clone()) {
        if supports_limits(s, &agent) {
            s.account_limits.remove(&agent);
            s.send(Command::LoadAccountLimits { agent });
        }
    }
}

/// The header meter: how full the context is (or the tokens spent when the
/// model's window is unknown). `None` when the session reports nothing.
pub fn meter(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    let info = s.session_by_id(asid)?;
    let (used, limit) = usage(s, info)?;
    let language = s.cfg.general.language;
    let mut line = row()
        .key("context-meter")
        .gap(6.0)
        .center_y()
        .px(8.0)
        .h(32.0)
        .fill(k.c.layer_01.alpha(0.7))
        .border(0.5, k.c.border_muted.alpha(0.3));
    let help = match limit {
        Some(limit) => {
            let fraction = used as f32 / limit as f32;
            line = line.child(bar(k, "meter-bar", fraction, 34.0, 4.0)).child(
                text(format!("{:.0}%", (fraction * 100.0).min(100.0)))
                    .font(k.ty.small_medium)
                    .color(if fraction >= 0.85 {
                        tone(k, fraction)
                    } else {
                        k.c.text_muted
                    }),
            );
            match language {
                Language::En => format!(
                    "Context: {} of {} tokens the model can hold",
                    compact(used),
                    compact(limit)
                ),
                _ => format!(
                    "{}: {} / {}",
                    crate::ui::copy::t(language, "Context:"),
                    compact(used),
                    compact(limit)
                ),
            }
        }
        None => {
            line = line.child(
                text(compact(used))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
            );
            tr_ref(language, "Tokens the model last read").to_string()
        }
    };
    let asid = asid.to_string();
    Some(
        k.interactive(line.radius(k.radius.lg))
            .cursor(Cursor::Pointer)
            .help(help)
            .label(tr_ref(language, "Context"))
            .on_press(move |s: &mut UiState| open(s, &asid)),
    )
}

fn heading(k: &Kit, label: &str) -> El {
    text(label.to_string())
        .font(k.ty.small_medium)
        .color(k.c.text_faint)
        .pt(6.0)
}

fn fact(k: &Kit, label: &str, value: String) -> El {
    row()
        .w_full()
        .gap(12.0)
        .child(
            text(label.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .grow(1.0),
        )
        .child(
            text(value)
                .font(k.ty.small_medium)
                .color(k.c.text_base)
                .single_line()
                .truncate_head()
                .shrink(),
        )
}

/// `in 3h 20m` until `at_ms`, from now.
fn until(language: Language, at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mins = at_ms.saturating_sub(now) / 60_000;
    let (d, h, m) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    let span = match language {
        Language::En if d > 0 => format!("{d}d {h}h"),
        Language::En if h > 0 => format!("{h}h {m}m"),
        Language::En => format!("{m}m"),
        _ if d > 0 => format!("{d}d {h}h"),
        _ if h > 0 => format!("{h}h {m}m"),
        _ => format!("{m}m"),
    };
    match language {
        Language::En => format!("Resets in {span}"),
        _ => format!("{span} {}", crate::ui::copy::t(language, "remaining")),
    }
}

fn remaining_fraction(window: &Value) -> Option<f32> {
    let remaining = window
        .get("remaining")?
        .as_f64()
        .filter(|n| n.is_finite())?;
    let limit = if window.get("unit").and_then(Value::as_str) == Some("percent") {
        100.0
    } else {
        window
            .get("limit")?
            .as_f64()
            .filter(|n| n.is_finite() && *n > 0.0)?
    };
    Some((remaining / limit).clamp(0.0, 1.0) as f32)
}

pub fn limits_section(s: &UiState, k: &Kit, agent: &str) -> Option<El> {
    if !supports_limits(s, agent) {
        return None;
    }
    let language = s.cfg.general.language;
    let t = |en| tr_ref(language, en);
    let mut block = col()
        .key("limits")
        .w_full()
        .gap(8.0)
        .child(heading(k, t("Account limits")));
    let Some(loaded) = s.account_limits.get(agent) else {
        return Some(block.child(crate::ui::art::ActivityMark::Compact.build(k)));
    };
    let Some(limits) = loaded.as_ref() else {
        return Some(block.child(crate::ui::muted(k, t("Could not read account limits"))));
    };
    if limits.get("status").and_then(Value::as_str) != Some("available") {
        let why = match limits.get("reason").and_then(Value::as_str) {
            Some("not_authenticated") => t("Not signed in on the host"),
            _ => t("Account limits temporarily unavailable"),
        };
        return Some(block.child(crate::ui::muted(k, why)));
    }
    if limits.get("ordinary_usage_allowed") == Some(&Value::Bool(false)) {
        block = block.child(
            text(t(
                "Included usage is restricted. Paid or alternative usage may still be available.",
            ))
            .font(k.ty.small)
            .color(k.c.warning.fg)
            .wrap(),
        );
    }
    let buckets = limits
        .get("limits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut any = false;
    for (bi, bucket) in buckets.iter().enumerate() {
        let name = bucket
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| bucket.get("plan_type").and_then(Value::as_str));
        if let Some(name) = name.filter(|_| buckets.len() > 1) {
            block = block.child(
                text(name.to_string())
                    .key(format!("b{bi}"))
                    .font(k.ty.small_medium)
                    .color(k.c.text_base),
            );
        }
        for (wi, w) in bucket
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            any = true;
            let label = w
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    w.get("window_duration_minutes")
                        .and_then(Value::as_u64)
                        .map(|m| match (m >= 1440, language) {
                            (true, Language::En) => format!("{} day window", m / 1440),
                            (true, _) => {
                                format!(
                                    "{} {}",
                                    m / 1440,
                                    crate::ui::copy::t(language, "day window")
                                )
                            }
                            (false, Language::En) => format!("{} hour window", m / 60),
                            (false, _) => {
                                format!(
                                    "{} {}",
                                    m / 60,
                                    crate::ui::copy::t(language, "hour window")
                                )
                            }
                        })
                })
                .unwrap_or_else(|| t("Quota").to_string());
            let unit = w.get("unit").and_then(Value::as_str).unwrap_or("");
            let remaining = w.get("remaining").and_then(Value::as_f64);
            let value = match (remaining, unit) {
                (Some(r), "percent") => match language {
                    Language::En => format!("{r:.0}% left"),
                    _ => format!("{r:.0}% {}", crate::ui::copy::t(language, "left")),
                },
                (Some(r), unit) => match language {
                    Language::En => format!("{r} {unit} left"),
                    _ => format!("{r} {unit} {}", crate::ui::copy::t(language, "left")),
                },
                (None, _) => t("Remaining unknown").to_string(),
            };
            let mut row_block = col()
                .key(format!("w{bi}-{wi}"))
                .w_full()
                .gap(10.0)
                .px(16.0)
                .py(14.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01.alpha(0.45))
                .child(fact(k, &label, value));
            if let Some(fraction) = remaining_fraction(w) {
                let color = tone(k, 1.0 - fraction);
                row_block = row_block.child(
                    row()
                        .key("remaining-bar")
                        .w_full()
                        .h(8.0)
                        .radius(k.radius.xs)
                        .fill(k.c.text_base.alpha(0.09))
                        .children(
                            (fraction > 0.0)
                                .then(|| progress_fill(k, color, 8.0).w(0.0).grow(fraction)),
                        )
                        .child(col().w(0.0).grow(1.0 - fraction).h(8.0)),
                );
            }
            if let Some(at) = w.get("resets_at_ms").and_then(Value::as_u64) {
                row_block = row_block.child(
                    text(until(language, at))
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                );
            }
            block = block.child(row_block);
        }
    }
    if !any {
        block = block.child(crate::ui::muted(k, t("No quota values reported")));
    }
    Some(block)
}

/// Content-sized details, anchored to this session's own footer.
pub fn popover(s: &UiState, k: &Kit, layer: &mut Vec<El>, asid: &str, trigger: El) -> El {
    let open = s.context_open.as_deref() == Some(asid);
    let mut popup = rune_kit::Popover::new(format!("session-context-{asid}"))
        .trigger(trigger)
        .placement(rune_kit::Placement::TopEnd)
        .gutter(8.0)
        .padding(0.0)
        .label(tr_ref(s.cfg.general.language, "Context"))
        .open(open)
        .on_close(|s: &mut UiState| s.context_open = None);
    if open {
        if let Some(body) = content(s, k, asid) {
            popup = popup.child(rune_kit::popover::fit_scroll(body).max_h(440.0));
        }
    }
    popup.build(k, layer)
}

fn content(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    let info = s.session_by_id(asid)?;
    let language = s.cfg.general.language;
    let t = |en| tr_ref(language, en);
    let close = |s: &mut UiState| s.context_open = None;
    let mut body = col()
        .key(format!("context-body-{asid}"))
        .w(316.0)
        .gap(8.0)
        .px(14.0)
        .py(12.0);
    body = body.child(
        row()
            .w_full()
            .center_y()
            .child(
                text(t("Context"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .grow(1.0),
            )
            .child(
                IconButton::new(&glyphs::CLOSE)
                    .variant(IconButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .label(t("Close"))
                    .help(t("Close"))
                    .on_press(close)
                    .build(k)
                    .key("context-close"),
            ),
    );
    match usage(s, info) {
        Some((used, Some(limit))) => {
            let fraction = used as f32 / limit as f32;
            body = body
                .child(
                    text(format!("{:.0}%", (fraction * 100.0).min(100.0)))
                        .font(rune::TextStyle {
                            size: 24.0,
                            line_height: 30.0,
                            ..k.ty.ui_medium
                        })
                        .color(tone(k, fraction)),
                )
                .child(bar(k, "context-bar", fraction, 288.0, 5.0))
                .child(crate::ui::muted(
                    k,
                    match language {
                        Language::En => format!(
                            "{} of {} tokens the model can hold",
                            compact(used),
                            compact(limit)
                        ),
                        _ => format!(
                            "{}: {} / {}",
                            crate::ui::copy::t(language, "Context:"),
                            compact(used),
                            compact(limit)
                        ),
                    },
                ));
        }
        Some((used, None)) => {
            body = body.child(crate::ui::muted(
                k,
                match language {
                    Language::En => {
                        format!(
                            "{} tokens the model last read; no window reported",
                            compact(used)
                        )
                    }
                    _ => format!(
                        "{} {}",
                        compact(used),
                        crate::ui::copy::t(language, "tokens read; no window reported")
                    ),
                },
            ));
        }
        None => body = body.child(crate::ui::muted(k, t("Not available"))),
    }
    if let Some(messages) = context_message_count(s, asid, info) {
        body = body.child(
            text(match language {
                Language::En if messages == 1 => "1 message in context".to_string(),
                Language::En => format!("{messages} messages in context"),
                _ => format!(
                    "{messages} {}",
                    crate::ui::copy::t(language, "messages in context")
                ),
            })
            .key("context-messages")
            .font(k.ty.small)
            .color(k.c.text_faint),
        );
    }
    body = body.child(heading(k, t("This session")));
    if let Some(model) = &info.model {
        let catalog_name = s
            .catalog_for_session(asid)
            .and_then(|cat| crate::ui::labels::catalog_model_info(cat, model))
            .map(|m| m.name.as_str());
        body = body.child(fact(
            k,
            t("Model"),
            crate::ui::labels::format_model_name(model, &model.model_id, catalog_name),
        ));
    }
    if let Some(dir) = &info.directory {
        body = body.child(fact(k, t("Project"), dir.clone()));
    }
    if let Some(tokens) = info.context_usage.as_ref().and_then(|c| c.tokens.as_ref()) {
        body = body
            .child(fact(k, t("Input"), compact(tokens.input)))
            .child(fact(k, t("Output"), compact(tokens.output)));
        if let Some(n) = tokens.cache_read.filter(|n| *n > 0) {
            body = body.child(fact(k, t("Cache read"), compact(n)));
        }
        if let Some(n) = tokens.cache_write.filter(|n| *n > 0) {
            body = body.child(fact(k, t("Cache write"), compact(n)));
        }
        if let Some(n) = tokens.reasoning.filter(|n| *n > 0) {
            body = body.child(fact(k, t("Reasoning"), compact(n)));
        }
    }
    if let Some(cost) = info.cost.filter(|c| *c > 0.0) {
        body = body.child(fact(k, t("Estimated cost"), format!("${cost:.2}")));
    }
    if let Some(limits) = limits_section(s, k, &info.agent_id) {
        body = body.child(limits);
    }
    let busy = matches!(
        info.status,
        AgentSessionStatus::Busy | AgentSessionStatus::Retry
    );
    let compacting = is_compacting(s, asid);
    let compact_asid = asid.to_string();
    body = body.child(
        col().key("context-actions").w_full().pt(6.0).child(
            Button::new(if compacting {
                t("Compacting…")
            } else {
                t("Compact context")
            })
            .variant(ButtonVariant::Submit)
            .disabled(busy || compacting)
            .on_press(move |s: &mut UiState| {
                s.context_open = None;
                s.compacting_sessions.insert(compact_asid.clone());
                s.send(Command::CompactSession {
                    asid: AgentSessionId(compact_asid.clone()),
                });
            })
            .build(k)
            .key("context-compact-btn")
            .w_full(),
        ),
    );
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::CatalogView;
    use crate::model::commands;
    use crate::ui::timeline::SessionTimeline;
    use muqun_gateway::agents::{AgentCatalog, CatalogDefaults, ModelInfo, ModelRef, TimelineItem};
    use serde_json::json;
    use std::sync::Arc;

    fn mk_session(id: &str) -> AgentSessionInfo {
        serde_json::from_value(json!({
            "asid": id,
            "agent_id": "codex",
            "title": id,
            "status": "idle",
            "model": { "provider_id": "openai", "model_id": "gpt-5.4" },
            "context_usage": {
                "total_tokens": 50_000,
                "tokens": { "input": 45_000, "output": 5_000 }
            },
            "updated_ms": 100
        }))
        .unwrap()
    }

    #[test]
    fn usage_falls_back_to_catalog_model_context_limit() {
        let (tx, _) = commands::channel();
        let mut s = UiState::new(tx);
        let info = mk_session("s1");
        s.sessions.push(info.clone());
        s.selected = Some("s1".into());
        assert_eq!(usage(&s, &info), Some((50_000, None)));

        let catalog = CatalogView {
            catalog: AgentCatalog {
                providers: vec![],
                models: vec![ModelInfo {
                    id: "gpt-5.4".into(),
                    name: "GPT-5.4".into(),
                    provider_id: "openai".into(),
                    family: None,
                    limit: Some(json!({ "context": 200_000 })),
                    variants: None,
                    cost: None,
                    enabled: true,
                    status: None,
                }],
                modes: vec![],
                commands: vec![],
                skills: vec![],
                mcp: vec![],
                defaults: CatalogDefaults {
                    model: Some(ModelRef {
                        provider_id: "openai".into(),
                        model_id: "gpt-5.4".into(),
                        variant: None,
                    }),
                    mode: None,
                },
            },
        };
        s.catalog = Some(Arc::new(catalog));
        assert_eq!(usage(&s, &info), Some((50_000, Some(200_000))));
    }

    #[test]
    fn context_message_count_respects_reported_limit_and_compaction_boundary() {
        let (tx, _) = commands::channel();
        let mut s = UiState::new(tx);
        let mut info = mk_session("s1");
        let items: Vec<TimelineItem> = vec![
            serde_json::from_value(json!({"id":"i1","message_id":"m1","role":"user","seq":1,"updated_ms":1,"part":{"type":"text","text":"old"}})).unwrap(),
            serde_json::from_value(json!({"id":"c1","message_id":"mc","role":"assistant","seq":2,"updated_ms":2,"part":{"type":"compaction","status":"completed"}})).unwrap(),
            serde_json::from_value(json!({"id":"i2","message_id":"m2","role":"user","seq":3,"updated_ms":3,"part":{"type":"text","text":"after"}})).unwrap(),
            serde_json::from_value(json!({"id":"i3","message_id":"m3","role":"assistant","seq":4,"updated_ms":4,"part":{"type":"text","text":"reply"}})).unwrap(),
        ];
        let mut tl = SessionTimeline::default();
        tl.upsert(items);
        s.timelines.insert("s1".into(), tl);
        assert_eq!(context_message_count(&s, "s1", &info), Some(2));

        info.context_usage.as_mut().unwrap().limit =
            Some(json!({ "context": 200_000, "messages": 7 }));
        assert_eq!(context_message_count(&s, "s1", &info), Some(7));
    }

    #[test]
    fn compact_context_button_renders_and_respects_compacting_state() {
        let (tx, _) = commands::channel();
        let mut s = UiState::new(tx);
        s.sessions.push(mk_session("s1"));
        s.selected = Some("s1".into());
        s.context_open = Some("s1".into());
        assert!(!is_compacting(&s, "s1"));
        s.compacting_sessions.insert("s1".into());
        assert!(is_compacting(&s, "s1"));

        let k = Kit::neutral(true);
        assert!(content(&s, &k, "s1").is_some());
    }
}
