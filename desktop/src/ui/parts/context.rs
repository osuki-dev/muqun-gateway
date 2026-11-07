//! How full a session's context is, and the agent's account limits (App
//! `agent-context-sheet` / `agent-account-limits-section`): a compact meter
//! for the session header and the sheet it opens. Only data the gateway
//! already gives: `context_usage` on the session, and
//! `GET /api/agents/{id}/account-limits` where the agent offers it.

use muqun_gateway::agents::AgentSessionInfo;
use rune::{col, icon, row, text, Color, Cursor, El};
use rune_kit::{Kit, Spinner};
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

/// What the model holds now and the most it can: (used, limit).
fn usage(info: &AgentSessionInfo) -> Option<(u64, Option<u64>)> {
    let usage = info.context_usage.as_ref()?;
    let limit = usage
        .limit
        .as_ref()
        .and_then(|l| l.get("context").or(Some(l)))
        .and_then(Value::as_u64)
        .filter(|l| *l > 0);
    (usage.total_tokens > 0).then_some((usage.total_tokens, limit))
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

fn bar(k: &Kit, key: &str, fraction: f32, width: f32, height: f32) -> El {
    let fraction = fraction.clamp(0.0, 1.0);
    row()
        .key(key.to_string())
        .w(width)
        .h(height)
        .radius(height / 2.0)
        .fill(k.alpha(k.c.text_base, 0.10))
        .clip_rounded(height / 2.0)
        .child(
            col()
                .w(width * fraction)
                .h(height)
                .radius(height / 2.0)
                .fill(tone(k, fraction)),
        )
}

fn supports_limits(s: &UiState, agent: &str) -> bool {
    s.agents
        .iter()
        .any(|a| a.id == agent && a.features.extra.get("accountLimits") == Some(&Value::Bool(true)))
}

/// Open the sheet for `asid`, asking for its agent's limits once.
pub fn open(s: &mut UiState, asid: &str) {
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
    let (used, limit) = usage(info)?;
    let language = s.cfg.general.language;
    let mut line = row()
        .key("context-meter")
        .gap(6.0)
        .center_y()
        .px(8.0)
        .h(26.0);
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
                _ => crate::ui::copy::localize(
                    language,
                    format!(
                        "上下文：模型可容纳 {} 中的 {}",
                        compact(limit),
                        compact(used)
                    ),
                ),
            }
        }
        None => {
            line = line.child(
                text(compact(used))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
            );
            tr_ref(
                language,
                "Tokens the model last read",
                "模型最近读取的 tokens",
            )
            .to_string()
        }
    };
    let asid = asid.to_string();
    Some(
        k.interactive(line.radius(k.radius.md))
            .cursor(Cursor::Pointer)
            .help(help)
            .label(tr_ref(language, "Context", "上下文"))
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
        _ if d > 0 => format!("{d} 天 {h} 小时"),
        _ if h > 0 => format!("{h} 小时 {m} 分"),
        _ => format!("{m} 分钟"),
    };
    match language {
        Language::En => format!("Resets in {span}"),
        _ => crate::ui::copy::localize(language, format!("{span}后重置")),
    }
}

fn limits_section(s: &UiState, k: &Kit, agent: &str) -> Option<El> {
    if !supports_limits(s, agent) {
        return None;
    }
    let language = s.cfg.general.language;
    let t = |en, zh| tr_ref(language, en, zh);
    let mut block = col()
        .key("limits")
        .w_full()
        .gap(8.0)
        .child(heading(k, t("Account limits", "账户额度")));
    let Some(loaded) = s.account_limits.get(agent) else {
        return Some(block.child(Spinner::new().size(14.0).build(k)));
    };
    let Some(limits) = loaded.as_ref() else {
        return Some(block.child(crate::ui::muted(
            k,
            t("Could not read account limits", "无法读取账户额度"),
        )));
    };
    if limits.get("status").and_then(Value::as_str) != Some("available") {
        let why = match limits.get("reason").and_then(Value::as_str) {
            Some("not_authenticated") => t("Not signed in on the host", "主机上未登录"),
            _ => t(
                "Account limits temporarily unavailable",
                "账户额度暂时不可用",
            ),
        };
        return Some(block.child(crate::ui::muted(k, why)));
    }
    if limits.get("ordinary_usage_allowed") == Some(&Value::Bool(false)) {
        block = block.child(
            text(t(
                "Included usage is restricted. Paid or alternative usage may still be available.",
                "套餐内额度已受限，付费或其他额度可能仍可使用。",
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
                                crate::ui::copy::localize(language, format!("{} 天周期", m / 1440))
                            }
                            (false, Language::En) => format!("{} hour window", m / 60),
                            (false, _) => {
                                crate::ui::copy::localize(language, format!("{} 小时周期", m / 60))
                            }
                        })
                })
                .unwrap_or_else(|| t("Quota", "额度").to_string());
            let unit = w.get("unit").and_then(Value::as_str).unwrap_or("");
            let remaining = w.get("remaining").and_then(Value::as_f64);
            let value = match (remaining, unit) {
                (Some(r), "percent") => match language {
                    Language::En => format!("{r:.0}% left"),
                    _ => crate::ui::copy::localize(language, format!("剩余 {r:.0}%")),
                },
                (Some(r), unit) => match language {
                    Language::En => format!("{r} {unit} left"),
                    _ => crate::ui::copy::localize(language, format!("剩余 {r} {unit}")),
                },
                (None, _) => t("Remaining unknown", "剩余未知").to_string(),
            };
            let mut row_block = col()
                .key(format!("w{bi}-{wi}"))
                .w_full()
                .gap(4.0)
                .child(fact(k, &label, value));
            if let (Some(r), "percent") = (remaining, unit) {
                // The bar shows what is used, filling toward the limit.
                row_block = row_block.child(bar(k, "used", (100.0 - r as f32) / 100.0, 300.0, 4.0));
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
        block = block.child(crate::ui::muted(
            k,
            t("No quota values reported", "没有报告额度数值"),
        ));
    }
    Some(block)
}

/// The sheet the meter opens, over the window's right side.
pub fn sheet(s: &UiState, k: &Kit) -> Option<El> {
    let asid = s.context_open.as_deref()?;
    let info = s.session_by_id(asid)?;
    let language = s.cfg.general.language;
    let t = |en, zh| tr_ref(language, en, zh);
    let close = |s: &mut UiState| s.context_open = None;
    let mut body = col()
        .key("context-body")
        .w_full()
        .gap(8.0)
        .px(18.0)
        .py(16.0);
    body = body.child(
        row()
            .w_full()
            .center_y()
            .child(
                text(t("Context", "上下文"))
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base)
                    .grow(1.0),
            )
            .child(
                k.interactive(
                    row()
                        .key("context-close")
                        .size(28.0, 28.0)
                        .center()
                        .radius(k.radius.md)
                        .child(icon(&glyphs::CLOSE, 14.0).color(k.c.icon_muted)),
                )
                .cursor(Cursor::Pointer)
                .label(t("Close", "关闭"))
                .on_press(close),
            ),
    );
    match usage(info) {
        Some((used, Some(limit))) => {
            let fraction = used as f32 / limit as f32;
            body = body
                .child(
                    text(format!("{:.0}%", (fraction * 100.0).min(100.0)))
                        .font(rune::TextStyle {
                            size: 28.0,
                            line_height: 34.0,
                            ..k.ty.ui_medium
                        })
                        .color(tone(k, fraction)),
                )
                .child(bar(k, "context-bar", fraction, 340.0, 6.0))
                .child(crate::ui::muted(
                    k,
                    match language {
                        Language::En => format!(
                            "{} of {} tokens the model can hold",
                            compact(used),
                            compact(limit)
                        ),
                        _ => crate::ui::copy::localize(
                            language,
                            format!(
                                "模型可容纳 {} tokens，已用 {}",
                                compact(limit),
                                compact(used)
                            ),
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
                    _ => crate::ui::copy::localize(
                        language,
                        format!("模型最近读取了 {} tokens；未报告上限", compact(used)),
                    ),
                },
            ));
        }
        None => body = body.child(crate::ui::muted(k, t("Not available", "不可用"))),
    }
    body = body.child(heading(k, t("This session", "本会话")));
    if let Some(model) = &info.model {
        body = body.child(fact(
            k,
            t("Model", "模型"),
            crate::ui::labels::format_model_name(model, &model.model_id, None),
        ));
    }
    if let Some(dir) = &info.directory {
        body = body.child(fact(k, t("Project", "项目"), dir.clone()));
    }
    if let Some(tokens) = info.context_usage.as_ref().and_then(|c| c.tokens.as_ref()) {
        body = body
            .child(fact(k, t("Input", "输入"), compact(tokens.input)))
            .child(fact(k, t("Output", "输出"), compact(tokens.output)));
        if let Some(n) = tokens.cache_read.filter(|n| *n > 0) {
            body = body.child(fact(k, t("Cache read", "缓存读取"), compact(n)));
        }
        if let Some(n) = tokens.cache_write.filter(|n| *n > 0) {
            body = body.child(fact(k, t("Cache write", "缓存写入"), compact(n)));
        }
        if let Some(n) = tokens.reasoning.filter(|n| *n > 0) {
            body = body.child(fact(k, t("Reasoning", "推理"), compact(n)));
        }
    }
    if let Some(cost) = info.cost.filter(|c| *c > 0.0) {
        body = body.child(fact(
            k,
            t("Estimated cost", "预估费用"),
            format!("${cost:.2}"),
        ));
    }
    if let Some(limits) = limits_section(s, k, &info.agent_id) {
        body = body.child(limits);
    }
    let panel = k.elevate(
        col()
            .key("context-panel")
            .w(380.0)
            .h_full()
            .fill(k.c.bg_base)
            .border(1.0, k.c.border_weak)
            .on_press(|_: &mut UiState| {})
            .child(body)
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            }),
        &k.elevation.overlay,
    );
    let panel = if s.cfg.appearance.reduced_motion {
        panel.transition(rune::Transition::opacity())
    } else {
        panel.transition(rune::Transition::offset(32.0, 0.0))
    };
    Some(
        row()
            .key("context-sheet")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .justify_end()
            .fill(k.alpha(k.c.black, 0.18))
            .on_press(close)
            .modal(true, close)
            .child(panel),
    )
}
