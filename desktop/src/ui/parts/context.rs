//! How full a session's context is, and the agent's account limits (App
//! `agent-context-sheet` / `agent-account-limits-section`): a compact meter
//! for the session footer and the sheet it opens, plus the shared account
//! quota block shown in Settings → Agents. Only data the gateway already
//! gives: `context_usage` / `tokens` / `git_info` on the session, and
//! `GET /api/agents/{id}/account-limits` where the agent offers it.

use std::collections::HashSet;

use muqun_gateway::agents::{
    AgentPart, AgentSessionId, AgentSessionInfo, AgentSessionStatus, CompactionStatus, TokensUsage,
};
use rune::{col, el, row, text, Color, Cursor, El};
use rune_kit::{Button, ButtonVariant, Kit};
use serde_json::Value;

use crate::config::Language;
use crate::model::commands::Command;
use crate::ui::copy::tr_ref;
use crate::ui::UiState;

/// Matches the official CLI status snapshot threshold (15 minutes).
const ACCOUNT_LIMIT_STALE_MS: u64 = 15 * 60_000;

/// `175k`, `1.2M`.
pub fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n if n >= 1000 => format!("{:.1}k", n as f64 / 1000.0),
        n => n.to_string(),
    }
}

/// Comma-grouped exact integer: `235517` -> `"235,517"`.
pub fn grouped(n: u64) -> String {
    group_digits(&n.to_string())
}

fn group_digits(digits: &str) -> String {
    let (neg, body) = match digits.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, digits),
    };
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    if neg {
        out.push('-');
    }
    let len = body.len();
    for (i, ch) in body.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Display grouping only: never convert exact decimal credit evidence to f64.
pub fn format_credits(value: &str) -> String {
    let mut parts = value.splitn(2, '.');
    let integer_raw = parts.next().unwrap_or("");
    let fraction_raw = parts.next().unwrap_or("");
    let integer = match integer_raw {
        "" => "0",
        "-" => "-0",
        other => other,
    };
    let grouped = group_digits(integer);
    let significant = fraction_raw.trim_end_matches('0');
    if significant.is_empty() {
        grouped
    } else {
        format!("{grouped}.{significant}")
    }
}

pub fn tokens_label(_language: Language, n: u64) -> String {
    format!("{} tokens", compact(n))
}

fn tokens_total(t: &TokensUsage) -> u64 {
    t.input
        .saturating_add(t.output)
        .saturating_add(t.reasoning.unwrap_or(0))
        .saturating_add(t.cache_read.unwrap_or(0))
        .saturating_add(t.cache_write.unwrap_or(0))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    if fraction >= 0.90 {
        k.c.danger.icon
    } else if fraction >= 0.70 {
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

fn full_bar(k: &Kit, key: &str, fraction: f32, height: f32) -> El {
    let fraction = fraction.clamp(0.0, 1.0);
    row()
        .key(key.to_string())
        .w_full()
        .h(height)
        .radius(height / 2.0)
        .fill(k.alpha(k.c.text_base, 0.09))
        .children((fraction > 0.0).then(|| {
            progress_fill(k, tone(k, fraction), height)
                .w(0.0)
                .grow(fraction)
        }))
        .child(col().w(0.0).grow(1.0 - fraction).h(height))
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

fn remaining_bar(k: &Kit, key: &str, fraction: f32) -> El {
    let fraction = fraction.clamp(0.0, 1.0);
    let color = tone(k, 1.0 - fraction);
    row()
        .key(key.to_string())
        .w_full()
        .h(8.0)
        .radius(k.radius.xs)
        .fill(k.c.text_base.alpha(0.09))
        .children((fraction > 0.0).then(|| progress_fill(k, color, 8.0).w(0.0).grow(fraction)))
        .child(col().w(0.0).grow(1.0 - fraction).h(8.0))
}

pub fn supports_limits(s: &UiState, agent: &str) -> bool {
    s.agents
        .iter()
        .any(|a| a.id == agent && a.features.extra.get("accountLimits") == Some(&Value::Bool(true)))
}

fn supports_compaction(s: &UiState, agent: &str) -> bool {
    s.agents
        .iter()
        .any(|a| a.id == agent && a.features.compaction)
}

/// Open the sheet for `asid`, refreshing its agent's limits without flashing blank.
pub fn open(s: &mut UiState, asid: &str) {
    if s.context_open.as_deref() == Some(asid) {
        s.context_open = None;
        return;
    }
    s.menu = None;
    s.context_open = Some(asid.to_string());
    if let Some(agent) = s.session_by_id(asid).map(|i| i.agent_id.clone()) {
        s.refresh_agent_account_limits(&agent, false);
    }
}

/// The footer meter: how full the context is (or the session tokens spent when
/// the model's window is unknown). `None` when the session reports nothing.
pub fn meter(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    let info = s.session_by_id(asid)?;
    let language = s.cfg.general.language;
    let mut line = row()
        .key("context-meter")
        .gap(6.0)
        .center_y()
        .px(8.0)
        .h(32.0)
        .fill(k.c.layer_01.alpha(0.7))
        .border(0.5, k.c.border_muted.alpha(0.3));
    let help = match usage(s, info) {
        Some((used, Some(limit))) => {
            let fraction = used as f32 / limit as f32;
            line = line.child(bar(k, "meter-bar", fraction, 34.0, 4.0)).child(
                text(format!("{:.0}%", (fraction * 100.0).min(100.0)))
                    .font(k.ty.small_medium)
                    .color(if fraction >= 0.70 {
                        tone(k, fraction)
                    } else {
                        k.c.text_muted
                    }),
            );
            context_occupancy_caption(language, used, limit)
        }
        Some((used, None)) => {
            line = line.child(
                text(compact(used))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
            );
            tr_ref(language, "Tokens the model last read").to_string()
        }
        None => {
            let spent = info.tokens.as_ref().map(tokens_total).filter(|n| *n > 0)?;
            line = line.child(
                text(compact(spent))
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
            );
            format!(
                "{}: {}",
                tr_ref(language, "Tokens this session"),
                grouped(spent)
            )
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

fn context_occupancy_caption(language: Language, used: u64, limit: u64) -> String {
    match language {
        Language::ZhCn => format!(
            "模型可容纳 {} 个 token，已用 {}",
            compact(limit),
            grouped(used)
        ),
        Language::ZhTw => format!(
            "模型可容納 {} 個 token，已用 {}",
            compact(limit),
            grouped(used)
        ),
        Language::En => format!(
            "{} of {} tokens the model can hold",
            grouped(used),
            compact(limit)
        ),
        _ => format!(
            "{} {} / {}",
            tr_ref(language, "Context:"),
            grouped(used),
            compact(limit)
        ),
    }
}

fn heading(k: &Kit, label: &str) -> El {
    text(label.to_string())
        .font(k.ty.small_medium)
        .color(k.c.text_faint)
        .pt(6.0)
}

fn divider(k: &Kit) -> El {
    el().w_full().h(1.0).fill(k.c.border_weak.alpha(0.55))
}

fn fact(k: &Kit, label: &str, value: String) -> El {
    row()
        .w_full()
        .min_w(0.0)
        .gap(10.0)
        .center_y()
        .child(
            text(label.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .grow(1.0)
                .shrink()
                .min_w(0.0),
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

fn fact_accent(k: &Kit, label: &str, value: String, color: Color) -> El {
    row()
        .w_full()
        .gap(12.0)
        .center_y()
        .child(
            text(label.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .grow(1.0),
        )
        .child(
            text(value)
                .font(k.ty.small_medium)
                .color(color)
                .single_line()
                .shrink(),
        )
}

/// Convert Unix epoch milliseconds to local calendar components
/// `(year, month 1..12, day 1..31, hour 0..23, minute 0..59, second 0..59)`.
#[cfg(unix)]
fn local_calendar(at_ms: u64) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let secs = i64::try_from(at_ms / 1000).ok()?;
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::localtime_r(&t, &mut tm) };
    if res.is_null() {
        return None;
    }
    Some((
        tm.tm_year + 1900,
        (tm.tm_mon + 1).max(1) as u32,
        tm.tm_mday.max(1) as u32,
        tm.tm_hour.max(0) as u32,
        tm.tm_min.max(0) as u32,
        tm.tm_sec.clamp(0, 59) as u32,
    ))
}

/// Portable UTC fallback for non-Unix builds.
#[cfg(not(unix))]
fn local_calendar(at_ms: u64) -> Option<(i32, u32, u32, u32, u32, u32)> {
    let secs = at_ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let second = (rem % 60) as u32;
    // Howard Hinnant civil-from-days algorithm (UTC).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    Some((year, m as u32, d as u32, hour, minute, second))
}

fn format_local_datetime(language: Language, at_ms: u64) -> Option<String> {
    let (y, mo, d, h, mi, s) = local_calendar(at_ms)?;
    Some(match language {
        Language::ZhCn | Language::ZhTw | Language::Ja => {
            format!("{y}/{mo:02}/{d:02} {h:02}:{mi:02}:{s:02}")
        }
        _ => format!("{y}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
    })
}

fn relative_countdown(language: Language, delta_ms: u64) -> String {
    let mins = delta_ms / 60_000;
    let (d, h, m) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    match language {
        Language::ZhCn => {
            if d > 0 {
                format!("{d}天 {h}小时后")
            } else if h > 0 {
                format!("{h}小时 {m}分钟后")
            } else if m > 0 {
                format!("{m}分钟后")
            } else {
                "不到1分钟".to_string()
            }
        }
        Language::ZhTw => {
            if d > 0 {
                format!("{d}天 {h}小時後")
            } else if h > 0 {
                format!("{h}小時 {m}分鐘後")
            } else if m > 0 {
                format!("{m}分鐘後")
            } else {
                "不到1分鐘".to_string()
            }
        }
        _ => {
            if d > 0 {
                format!("in {d}d {h}h")
            } else if h > 0 {
                format!("in {h}h {m}m")
            } else if m > 0 {
                format!("in {m}m")
            } else {
                "in <1m".to_string()
            }
        }
    }
}

/// Format an exact local reset date/time alongside a relative countdown when
/// the reset timestamp is in the future.
pub fn format_reset_time(language: Language, at_ms: u64, current_ms: u64) -> String {
    let prefix = tr_ref(language, "Resets at");
    let Some(dt) = format_local_datetime(language, at_ms) else {
        return tr_ref(language, "Reset time not reported").to_string();
    };
    if at_ms > current_ms {
        let rel = relative_countdown(language, at_ms - current_ms);
        match language {
            Language::ZhCn | Language::ZhTw => format!("{prefix}：{dt}（{rel}）"),
            _ => format!("{prefix} {dt} ({rel})"),
        }
    } else {
        match language {
            Language::ZhCn | Language::ZhTw => format!("{prefix}：{dt}"),
            _ => format!("{prefix} {dt}"),
        }
    }
}

fn format_window_duration(language: Language, minutes: u64) -> String {
    if minutes.is_multiple_of(10080) {
        let n = minutes / 10080;
        match language {
            Language::ZhCn => format!("{n} 周"),
            Language::ZhTw => format!("{n} 週"),
            Language::En if n == 1 => "1 week".to_string(),
            Language::En => format!("{n} weeks"),
            _ => format!("{n} {}", tr_ref(language, "weeks")),
        }
    } else if minutes.is_multiple_of(1440) {
        let n = minutes / 1440;
        match language {
            Language::ZhCn | Language::ZhTw => format!("{n} 天"),
            Language::En if n == 1 => "1 day".to_string(),
            Language::En => format!("{n} days"),
            _ => format!("{n} {}", tr_ref(language, "days")),
        }
    } else if minutes.is_multiple_of(60) {
        let n = minutes / 60;
        match language {
            Language::ZhCn => format!("{n} 小时"),
            Language::ZhTw => format!("{n} 小時"),
            Language::En if n == 1 => "1 hour".to_string(),
            Language::En => format!("{n} hours"),
            _ => format!("{n} {}", tr_ref(language, "hours")),
        }
    } else {
        match language {
            Language::ZhCn => format!("{minutes} 分钟"),
            Language::ZhTw => format!("{minutes} 分鐘"),
            Language::En if minutes == 1 => "1 minute".to_string(),
            Language::En => format!("{minutes} minutes"),
            _ => format!("{minutes} {}", tr_ref(language, "minutes")),
        }
    }
}

fn format_remaining_value(language: Language, remaining: Option<f64>, unit: &str) -> String {
    let Some(r) = remaining.filter(|v| v.is_finite()) else {
        return tr_ref(language, "Remaining unknown").to_string();
    };
    let num = if (r.fract()).abs() < 1e-6 {
        grouped(r.round().max(0.0) as u64)
    } else {
        format!("{r:.1}")
    };
    let amount = if unit == "percent" {
        format!("{num}%")
    } else if unit.is_empty() {
        num
    } else {
        format!("{num} {unit}")
    };
    match language {
        Language::ZhCn => format!("剩余 {amount}"),
        Language::ZhTw => format!("剩餘 {amount}"),
        Language::En => format!("{amount} remaining"),
        _ => format!("{amount} {}", tr_ref(language, "remaining")),
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

/// Metadata alone must not leave an empty quota heading (matches App `hasAccountLimitValues`).
fn has_account_limit_values(bucket: &Value) -> bool {
    let has_windows = bucket
        .get("windows")
        .and_then(Value::as_array)
        .is_some_and(|w| !w.is_empty());
    let has_credits = bucket
        .get("credits")
        .and_then(Value::as_object)
        .is_some_and(|obj| obj.values().any(|v| !v.is_null()));
    let has_individual = bucket
        .get("individual_limit")
        .and_then(Value::as_object)
        .is_some_and(|obj| obj.values().any(|v| !v.is_null()));
    let has_spend_control = bucket
        .get("spend_control_reached")
        .is_some_and(|v| !v.is_null());
    let has_reason = bucket
        .get("limit_reached_reason")
        .is_some_and(|v| !v.is_null());
    has_windows || has_credits || has_individual || has_spend_control || has_reason
}

fn format_credits_state(language: Language, credits: &Value) -> String {
    let t = |en| tr_ref(language, en);
    if credits.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return t("Unlimited").to_string();
    }
    if let Some(balance) = credits.get("balance").and_then(Value::as_str) {
        return format_credits(balance);
    }
    match credits.get("has_credits").and_then(Value::as_bool) {
        Some(true) => t("Available").to_string(),
        Some(false) => t("Not available").to_string(),
        None => t("Not reported").to_string(),
    }
}

fn plan_badge(k: &Kit, plan: &str) -> El {
    row()
        .key("plan-badge")
        .px(7.0)
        .py(2.0)
        .radius(k.radius.sm)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_weak.alpha(0.5))
        .center_y()
        .child(
            text(plan.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_muted),
        )
}

/// Shared account quota block used in both the session Context popover and
/// Settings → Agents.
pub fn limits_section(s: &UiState, k: &Kit, agent: &str) -> Option<El> {
    if !supports_limits(s, agent) {
        return None;
    }
    let language = s.cfg.general.language;
    let t = |en| tr_ref(language, en);
    let current_ms = now_ms();
    let loading = s.account_limits_loading.contains(agent);
    let loaded = s.account_limits.get(agent);
    let limits_opt = loaded.and_then(|v| v.as_ref());
    let available = limits_opt
        .and_then(|l| (l.get("status").and_then(Value::as_str) == Some("available")).then_some(l));
    let buckets: Vec<&Value> = available
        .and_then(|l| l.get("limits").and_then(Value::as_array))
        .map(|arr| arr.iter().filter(|b| has_account_limit_values(b)).collect())
        .unwrap_or_default();
    let single_plan = if buckets.len() == 1 {
        buckets[0]
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
    } else {
        None
    };

    let refresh_agent = agent.to_string();
    let refresh_control = if loading {
        el().key("limits-refreshing")
            .size(28.0, 28.0)
            .center()
            .child(crate::ui::art::ActivityMark::Compact.build(k))
    } else {
        crate::ui::icons::header_button(&crate::ui::icons::REFRESH, t("Refresh account limits"))
            .on_press(move |s: &mut UiState| {
                s.refresh_agent_account_limits(&refresh_agent, true);
            })
            .build(k)
            .key(format!("limits-refresh-{agent}"))
    };

    let mut header = row()
        .key("limits-header")
        .w_full()
        .center_y()
        .gap(8.0)
        .child(
            text(t("Account limits").to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_faint)
                .grow(1.0),
        );
    if let Some(plan) = single_plan {
        header = header.child(plan_badge(k, plan));
    }
    header = header.child(refresh_control);

    let mut block = col().key("limits").w_full().gap(8.0).child(header);

    let Some(loaded) = loaded else {
        return Some(block.child(crate::ui::art::ActivityMark::Compact.build(k)));
    };
    let Some(limits) = loaded.as_ref() else {
        return Some(block.child(crate::ui::muted(k, t("Could not read account limits"))));
    };
    if let Some(observed_at_ms) = limits
        .get("observed_at_ms")
        .and_then(Value::as_u64)
        .filter(|t| *t > 0)
    {
        if current_ms.saturating_sub(observed_at_ms) >= ACCOUNT_LIMIT_STALE_MS {
            block = block.child(
                text(t("Usage snapshot is stale").to_string())
                    .key("limits-stale")
                    .font(k.ty.small)
                    .color(k.c.warning.fg),
            );
        }
    }
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
    if buckets.is_empty() {
        return Some(block.child(crate::ui::muted(k, t("No quota values reported"))));
    }

    let reset_credits = limits.get("reset_credits").filter(|v| v.is_object());
    let reset_count = reset_credits
        .and_then(|rc| rc.get("available_count"))
        .and_then(Value::as_u64);

    for (bi, bucket) in buckets.iter().enumerate() {
        let heading_label = bucket
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| (buckets.len() > 1).then(|| format!("{} {}", t("Quota"), bi + 1)));
        let bucket_plan = (buckets.len() > 1)
            .then(|| {
                bucket
                    .get("plan_type")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
            })
            .flatten();
        if heading_label.is_some() || bucket_plan.is_some() {
            let mut sub = row()
                .key(format!("b{bi}"))
                .w_full()
                .center_y()
                .gap(8.0)
                .child(
                    text(heading_label.unwrap_or_else(|| t("Plan").to_string()))
                        .font(k.ty.small_medium)
                        .color(k.c.text_base)
                        .grow(1.0),
                );
            if let Some(plan) = bucket_plan {
                sub = sub.child(plan_badge(k, plan));
            }
            block = block.child(sub);
        }
        if let Some(model_id) = bucket
            .get("model_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            block = block.child(fact(k, t("Model"), model_id.to_string()));
        }

        for (wi, w) in bucket
            .get("windows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let explicit_label = w
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let duration = w
                .get("window_duration_minutes")
                .and_then(Value::as_u64)
                .filter(|m| *m > 0)
                .map(|m| format_window_duration(language, m));
            let label = explicit_label
                .map(str::to_string)
                .or_else(|| duration.clone())
                .unwrap_or_else(|| t("Period not reported").to_string());
            let unit = w.get("unit").and_then(Value::as_str).unwrap_or("");
            let remaining = w.get("remaining").and_then(Value::as_f64);
            let value = format_remaining_value(language, remaining, unit);
            let reset_text = match w.get("resets_at_ms").and_then(Value::as_u64) {
                Some(at) => format_reset_time(language, at, current_ms),
                None => t("Reset time not reported").to_string(),
            };
            let footnote = match (explicit_label, duration) {
                (Some(_), Some(dur)) => format!("{dur} · {reset_text}"),
                _ => reset_text,
            };

            let mut row_block = col()
                .key(format!("w{bi}-{wi}"))
                .w_full()
                .gap(8.0)
                .px(14.0)
                .py(12.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01.alpha(0.45))
                .border(1.0, k.c.border_weak.alpha(0.35))
                .child(fact(k, &label, value));
            if let Some(fraction) = remaining_fraction(w) {
                row_block = row_block.child(remaining_bar(k, "remaining-bar", fraction));
            }
            row_block =
                row_block.child(text(footnote).font(k.ty.small).color(k.c.text_muted).wrap());
            block = block.child(row_block);
        }

        let credits = bucket.get("credits").filter(|v| v.is_object());
        let bucket_reset_count = (bi == 0).then_some(reset_count).flatten();
        if credits.is_some() || bucket_reset_count.is_some() {
            let mut summary = col()
                .key(format!("b{bi}-summary"))
                .w_full()
                .gap(6.0)
                .px(14.0)
                .py(10.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01.alpha(0.35))
                .border(1.0, k.c.border_weak.alpha(0.3));
            if let Some(c) = credits {
                summary = summary.child(fact(k, t("Credits"), format_credits_state(language, c)));
            }
            if let Some(count) = bucket_reset_count {
                summary = summary.child(fact(k, t("Available resets"), grouped(count)));
            }
            if bi == 0 {
                if let Some(details) = reset_credits
                    .and_then(|rc| rc.get("credits"))
                    .and_then(Value::as_array)
                {
                    for (ri, item) in details.iter().take(4).enumerate() {
                        let title = item
                            .get("title")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("reset_type").and_then(Value::as_str))
                            .unwrap_or(t("Available resets"));
                        let detail = match item.get("expires_at_ms").and_then(Value::as_u64) {
                            Some(exp) => format_local_datetime(language, exp)
                                .map(|dt| format!("{} {dt}", t("Expires")))
                                .unwrap_or_default(),
                            None => item
                                .get("status")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        };
                        if !detail.is_empty() {
                            summary = summary
                                .child(fact(k, title, detail).key(format!("reset-detail-{ri}")));
                        }
                    }
                }
            }
            block = block.child(summary);
        }

        if let Some(monthly) = bucket.get("individual_limit").filter(|v| v.is_object()) {
            let remaining_pct = monthly
                .get("remaining_percent")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite());
            let used_s = monthly
                .get("used")
                .and_then(Value::as_str)
                .map(format_credits)
                .unwrap_or_else(|| t("Not reported").to_string());
            let limit_s = monthly
                .get("limit")
                .and_then(Value::as_str)
                .map(format_credits)
                .unwrap_or_else(|| t("Not reported").to_string());
            let mut mblock = col()
                .key(format!("b{bi}-monthly"))
                .w_full()
                .gap(8.0)
                .px(14.0)
                .py(12.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_01.alpha(0.45))
                .border(1.0, k.c.border_weak.alpha(0.35))
                .child(fact(
                    k,
                    t("Monthly credit limit"),
                    format_remaining_value(language, remaining_pct, "percent"),
                ));
            if let Some(pct) = remaining_pct {
                mblock = mblock.child(remaining_bar(k, "monthly-bar", (pct / 100.0) as f32));
            }
            mblock = mblock.child(fact(k, t("Credits used"), format!("{used_s} / {limit_s}")));
            if let Some(at) = monthly.get("resets_at_ms").and_then(Value::as_u64) {
                mblock = mblock.child(
                    text(format_reset_time(language, at, current_ms))
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap(),
                );
            }
            block = block.child(mblock);
        }

        if bucket.get("spend_control_reached") == Some(&Value::Bool(true)) {
            block = block.child(fact_accent(
                k,
                t("Spend control"),
                t("Limit reached").to_string(),
                k.c.warning.fg,
            ));
        }
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
            popup = popup.child(rune_kit::popover::fit_scroll(body).max_h(560.0));
        }
    }
    popup.build(k, layer)
}

fn format_variant(variant: &str) -> String {
    let v = variant.trim();
    if v.is_empty() {
        return String::new();
    }
    let mut chars = v.chars();
    match chars.next() {
        Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn display_model_name(s: &UiState, asid: &str, info: &AgentSessionInfo) -> Option<String> {
    let model = info.model.as_ref()?;
    let catalog_name = s
        .catalog_for_session(asid)
        .and_then(|cat| crate::ui::labels::catalog_model_info(cat, model))
        .map(|m| m.name.as_str());
    let base = crate::ui::labels::format_model_name(model, &model.model_id, catalog_name);
    if let Some(variant) = model
        .variant
        .as_deref()
        .map(format_variant)
        .filter(|v| !v.is_empty())
    {
        if !base
            .to_ascii_lowercase()
            .contains(&variant.to_ascii_lowercase())
        {
            return Some(format!("{base} · {variant}"));
        }
    }
    Some(base)
}

fn format_cost(language: Language, cost: f64) -> String {
    if cost == 0.0 {
        tr_ref(language, "Free").to_string()
    } else {
        format!("${cost:.4}")
    }
}

fn content(s: &UiState, k: &Kit, asid: &str) -> Option<El> {
    let info = s.session_by_id(asid)?;
    let language = s.cfg.general.language;
    let t = |en| tr_ref(language, en);
    let close = |s: &mut UiState| s.context_open = None;
    let mut body = col()
        .key(format!("context-body-{asid}"))
        .w(316.0)
        .gap(10.0)
        .px(14.0)
        .py(12.0);

    // Header: title + session caption + close.
    let mut title_col = col().grow(1.0).shrink().gap(2.0).child(
        text(t("Context").to_string())
            .font(k.ty.ui_medium)
            .color(k.c.text_base),
    );
    let caption = info.title.trim();
    if !caption.is_empty() && caption != info.asid.0 {
        title_col = title_col.child(
            text(caption.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .single_line()
                .truncate(),
        );
    }
    body = body.child(
        row().w_full().center_y().gap(8.0).child(title_col).child(
            crate::ui::icons::header_button(k.glyph.close, t("Close"))
                .on_press(close)
                .build(k)
                .key("context-close"),
        ),
    );

    // Context occupancy block.
    match usage(s, info) {
        Some((used, Some(limit))) => {
            let fraction = (used as f32 / limit as f32).clamp(0.0, 1.0);
            let pct_color = tone(k, fraction);
            body = body
                .child(
                    row()
                        .w_full()
                        .center_y()
                        .child(
                            text(t("Context used").to_string())
                                .font(k.ty.small_medium)
                                .color(k.c.text_base)
                                .grow(1.0),
                        )
                        .child(
                            text(format!(
                                "{:.1}%",
                                (used as f64 / limit as f64 * 100.0).min(100.0)
                            ))
                            .font(k.ty.small_medium)
                            .color(pct_color),
                        ),
                )
                .child(full_bar(k, "context-bar", fraction, 5.0))
                .child(crate::ui::muted(
                    k,
                    context_occupancy_caption(language, used, limit),
                ));
        }
        Some((used, None)) => {
            body = body
                .child(fact(
                    k,
                    t("Context used"),
                    t("no window reported for this model").to_string(),
                ))
                .child(crate::ui::muted(
                    k,
                    match language {
                        Language::En => format!("{} tokens the model last read", grouped(used)),
                        _ => format!("{} {}", grouped(used), t("tokens read; no window reported")),
                    },
                ));
        }
        None => {
            body = body.child(fact(k, t("Context used"), t("Not available").to_string()));
        }
    }
    if let Some(messages) = context_message_count(s, asid, info) {
        body = body.child(
            text(match language {
                Language::En if messages == 1 => "1 message in context".to_string(),
                Language::En => format!("{messages} messages in context"),
                _ => format!("{messages} {}", t("messages in context")),
            })
            .key("context-messages")
            .font(k.ty.small)
            .color(k.c.text_faint),
        );
    }

    // Account limits section (matches App order: right under context occupancy).
    if let Some(limits) = limits_section(s, k, &info.agent_id) {
        body = body.child(divider(k)).child(limits);
    }

    // This session facts & cumulative token spend.
    body = body.child(divider(k)).child(heading(k, t("This session")));
    if let Some(cost) = info.cost.filter(|c| c.is_finite() && *c >= 0.0) {
        body = body.child(fact_accent(
            k,
            t("Estimated cost"),
            format_cost(language, cost),
            k.c.icon_accent,
        ));
    }
    if let Some(dir) = &info.directory {
        body = body.child(fact(k, t("Project"), dir.clone()));
    }
    if let Some(model_name) = display_model_name(s, asid, info) {
        body = body.child(fact(k, t("Model"), model_name));
    }
    if let Some(tokens) = info.tokens.as_ref() {
        let total = tokens_total(tokens);
        body = body
            .child(fact(k, t("Tokens this session"), grouped(total)))
            .child(fact(k, t("Input"), grouped(tokens.input)))
            .child(fact(k, t("Output"), grouped(tokens.output)));
        if let Some(n) = tokens.reasoning {
            body = body.child(fact(k, t("Reasoning"), grouped(n)));
        }
        if let Some(n) = tokens.cache_read {
            body = body.child(fact(k, t("Cache read"), grouped(n)));
        }
        if let Some(n) = tokens.cache_write {
            body = body.child(fact(k, t("Cache write"), grouped(n)));
        }
    } else if let Some(tokens) = info.context_usage.as_ref().and_then(|c| c.tokens.as_ref()) {
        body = body
            .child(fact(k, t("Input"), grouped(tokens.input)))
            .child(fact(k, t("Output"), grouped(tokens.output)));
        if let Some(n) = tokens.reasoning {
            body = body.child(fact(k, t("Reasoning"), grouped(n)));
        }
        if let Some(n) = tokens.cache_read {
            body = body.child(fact(k, t("Cache read"), grouped(n)));
        }
        if let Some(n) = tokens.cache_write {
            body = body.child(fact(k, t("Cache write"), grouped(n)));
        }
    }

    // Git snapshot (captured by producer on session metadata).
    if let Some(git) = info.git_info.as_ref() {
        let has_branch = git.branch.as_deref().is_some_and(|b| !b.is_empty());
        let has_sha = git.sha.as_deref().is_some_and(|s| !s.is_empty());
        if has_branch || has_sha {
            body = body.child(divider(k)).child(heading(k, t("Git snapshot")));
            if let Some(branch) = git.branch.as_deref().filter(|b| !b.is_empty()) {
                body = body.child(fact(k, t("Branch"), branch.to_string()));
            }
            if let Some(sha) = git.sha.as_deref().filter(|s| !s.is_empty()) {
                let short: String = sha.chars().take(12).collect();
                body = body.child(fact(k, t("Commit"), short));
            }
        }
    }

    // Compaction action (strictly gated on agent capability).
    if supports_compaction(s, &info.agent_id) {
        let busy = matches!(
            info.status,
            AgentSessionStatus::Busy | AgentSessionStatus::Retry
        );
        let compacting = is_compacting(s, asid);
        let compact_asid = asid.to_string();
        body = body.child(
            col().key("context-actions").w_full().pt(4.0).child(
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
    }

    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::CatalogView;
    use crate::model::commands;
    use crate::ui::timeline::SessionTimeline;
    use muqun_gateway::agents::{AgentCatalog, CatalogDefaults, ModelInfo, ModelRef, TimelineItem};
    use muqun_gateway::platform::discovery::{
        AgentAvailability, AgentDiscoveryInfo, AgentFeatures,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn mk_session(id: &str) -> AgentSessionInfo {
        serde_json::from_value(json!({
            "asid": id,
            "agent_id": "codex",
            "title": "Report progress",
            "status": "idle",
            "directory": "/home/ryu/Work/osuki",
            "model": { "provider_id": "openai", "model_id": "gpt-5.4", "variant": "medium" },
            "context_usage": {
                "total_tokens": 50_000,
                "tokens": { "input": 45_000, "output": 5_000 }
            },
            "tokens": {
                "input": 10_378_975,
                "output": 618_129,
                "reasoning": 213_973,
                "cache_read": 956_003_712,
                "cache_write": 0
            },
            "git_info": {
                "branch": "feat/desktop-refactor-v2",
                "sha": "25b0902abcdef123456789"
            },
            "updated_ms": 100
        }))
        .unwrap()
    }

    fn mk_codex_agent(compaction: bool, limits: bool) -> AgentDiscoveryInfo {
        let mut features = AgentFeatures {
            streaming: true,
            reasoning_effort: true,
            model_selection: true,
            tool_approvals: true,
            worktrees: false,
            revert: false,
            staged_revert: false,
            inbox: false,
            modes: false,
            skills: false,
            slash_commands: false,
            compaction,
            background_shells: false,
            attachments: false,
            attachments_by_path: false,
            extra: Default::default(),
        };
        if limits {
            features
                .extra
                .insert("accountLimits".into(), Value::Bool(true));
        }
        AgentDiscoveryInfo {
            id: "codex".into(),
            kind: "codex".into(),
            name: "Codex".into(),
            status: AgentAvailability::Connected,
            enabled: true,
            endpoint: None,
            version: Some("0.172.0".into()),
            reason: None,
            models: Vec::new(),
            modes: Vec::new(),
            features,
        }
    }

    #[test]
    fn grouped_and_credits_preserve_exact_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(235_517), "235,517");
        assert_eq!(grouped(967_214_789), "967,214,789");
        assert_eq!(format_credits("13905.2351075000"), "13,905.2351075");
        assert_eq!(format_credits("10000.0000"), "10,000");
        assert_eq!(format_credits("-42.50"), "-42.5");
        assert_eq!(format_credits(".250"), "0.25");
    }

    #[test]
    fn window_duration_prefers_weeks_days_hours_and_minutes() {
        assert_eq!(format_window_duration(Language::En, 10080), "1 week");
        assert_eq!(format_window_duration(Language::ZhCn, 10080), "1 周");
        assert_eq!(format_window_duration(Language::En, 300), "5 hours");
        assert_eq!(format_window_duration(Language::ZhCn, 300), "5 小时");
        assert_eq!(format_window_duration(Language::En, 45), "45 minutes");
        assert_eq!(format_window_duration(Language::ZhCn, 45), "45 分钟");
    }

    #[test]
    fn reset_time_formats_exact_date_and_optional_countdown() {
        let now = 1_795_746_000_000u64;
        let future = now + 6 * 86_400_000 + 7 * 3_600_000;
        let zh = format_reset_time(Language::ZhCn, future, now);
        assert!(zh.contains("6天 7小时后"), "{zh}");
        assert!(zh.contains(':'), "{zh}");
        let en_past = format_reset_time(Language::En, now - 60_000, now);
        assert!(en_past.starts_with("Resets at "), "{en_past}");
        assert!(!en_past.contains("(in "), "{en_past}");
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
        assert_eq!(
            display_model_name(&s, "s1", &info).as_deref(),
            Some("GPT-5.4 · Medium")
        );
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
    fn compact_context_and_full_limits_render_with_capabilities() {
        let (tx, _) = commands::channel();
        let mut s = UiState::new(tx);
        s.agents.push(mk_codex_agent(true, true));
        s.sessions.push(mk_session("s1"));
        s.selected = Some("s1".into());
        s.context_open = Some("s1".into());
        s.account_limits.insert(
            "codex".into(),
            Some(json!({
                "status": "available",
                "reason": null,
                "observed_at_ms": now_ms(),
                "ordinary_usage_allowed": true,
                "reset_credits": { "available_count": 0, "credits": [] },
                "limits": [{
                    "id": "codex",
                    "name": null,
                    "plan_type": "pro",
                    "credits": { "has_credits": true, "unlimited": false, "balance": "13905.2351075000" },
                    "windows": [{
                        "id": "primary",
                        "label": null,
                        "unit": "percent",
                        "used": 33.0,
                        "limit": 100.0,
                        "remaining": 67.0,
                        "window_duration_minutes": 10080,
                        "resets_at_ms": now_ms() + 86_400_000
                    }]
                }]
            })),
        );
        assert!(!is_compacting(&s, "s1"));
        s.compacting_sessions.insert("s1".into());
        assert!(is_compacting(&s, "s1"));

        let k = Kit::neutral(true);
        assert!(content(&s, &k, "s1").is_some());
        assert!(limits_section(&s, &k, "codex").is_some());
    }
}
