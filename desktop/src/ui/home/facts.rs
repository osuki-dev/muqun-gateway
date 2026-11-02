//! What Home says, as plain functions of the view state and the clock, so
//! every number on the page is testable without a window: the local date and
//! greeting, the one-line stats, today's usage, the layout breakpoints and
//! whether this is a first run.

use muqun_gateway::agents::{AgentSessionInfo, AgentSessionStatus, PermissionRequest};

use crate::config::General;
use crate::ui::sessions::DayGroup;

/// A local calendar moment, from the epoch and the zone offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    /// 1–12.
    pub month: u32,
    /// 1–31.
    pub day: u32,
    /// 0 = Sunday … 6 = Saturday.
    pub weekday: u32,
    pub hour: u32,
}

impl LocalTime {
    pub fn at(now_ms: u64, utc_offset_s: i64) -> Self {
        let secs = now_ms as i64 / 1000 + utc_offset_s;
        let days = secs.div_euclid(86_400);
        let hour = (secs.rem_euclid(86_400) / 3600) as u32;
        // 1970-01-01 was a Thursday.
        let weekday = (days + 4).rem_euclid(7) as u32;
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            weekday,
            hour,
        }
    }
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a
/// proleptic Gregorian (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The part of the day the greeting names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayPart {
    Morning,
    Afternoon,
    Evening,
    Night,
}

impl DayPart {
    pub fn of(hour: u32) -> Self {
        match hour {
            5..=11 => Self::Morning,
            12..=17 => Self::Afternoon,
            18..=22 => Self::Evening,
            _ => Self::Night,
        }
    }
}

/// The counts the hero's second line reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Sessions whose agent is working right now.
    pub working: usize,
    /// Sessions with a permission waiting for the owner.
    pub waiting: usize,
    /// Sessions touched today, local time.
    pub today: usize,
}

impl Stats {
    pub fn of(
        sessions: &[AgentSessionInfo],
        attention: &[PermissionRequest],
        now_ms: u64,
        utc_offset_s: i64,
    ) -> Self {
        let mut waiting: Vec<&str> = attention.iter().map(|p| p.asid.0.as_str()).collect();
        waiting.sort_unstable();
        waiting.dedup();
        Self {
            working: sessions
                .iter()
                .filter(|s| {
                    matches!(
                        s.status,
                        AgentSessionStatus::Busy | AgentSessionStatus::Retry
                    )
                })
                .count(),
            waiting: waiting.len(),
            today: sessions
                .iter()
                .filter(|s| DayGroup::of(s.updated_ms, now_ms, utc_offset_s) == DayGroup::Today)
                .count(),
        }
    }

    /// Nothing worth a sentence.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.working == 0 && self.waiting == 0 && self.today == 0
    }
}

/// Today's spend, summed over the sessions touched today. Each session's
/// figures are its own running totals, as the agents report them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
}

impl Usage {
    pub fn today(sessions: &[AgentSessionInfo], now_ms: u64, utc_offset_s: i64) -> Self {
        let mut usage = Self::default();
        for s in sessions
            .iter()
            .filter(|s| DayGroup::of(s.updated_ms, now_ms, utc_offset_s) == DayGroup::Today)
        {
            if let Some(t) = &s.tokens {
                let total = t.input + t.output + t.reasoning.unwrap_or(0);
                *usage.tokens.get_or_insert(0) += total;
            }
            if let Some(cost) = s.cost.filter(|c| c.is_finite() && *c > 0.0) {
                *usage.cost.get_or_insert(0.0) += cost;
            }
        }
        usage
    }
}

/// `987`, `12.3k`, `128k`, `4.2M`.
pub fn compact_count(n: u64) -> String {
    let scaled = |v: f64, unit: &str| {
        if v < 100.0 {
            format!("{:.1}{unit}", v).replace(".0", "")
        } else {
            format!("{v:.0}{unit}")
        }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => scaled(n as f64 / 1e3, "k"),
        _ => scaled(n as f64 / 1e6, "M"),
    }
}

/// `s` in at most `max` characters, the middle replaced by `…` -- for
/// paths, whose start and end both say which one it is.
pub fn middle_ellipsis(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max < 3 {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep * 2 / 5;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Window-width breakpoints (§6.0 "响应").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub hero_height: f32,
    /// The side column sits beside the main one, else below it.
    pub two_columns: bool,
}

pub const WIDE_FROM: f32 = 1280.0;
pub const TWO_COLUMNS_FROM: f32 = 1100.0;
pub const NARROW_BELOW: f32 = 960.0;

impl Layout {
    pub fn for_window(width: f32) -> Self {
        Self {
            hero_height: if width >= WIDE_FROM {
                240.0
            } else if width >= NARROW_BELOW {
                180.0
            } else {
                140.0
            },
            two_columns: width >= TWO_COLUMNS_FROM,
        }
    }
}

/// The first-run state: nothing to continue and no session ever started
/// from this app. Before the agents are known nothing is decided.
pub fn first_run(agents_known: bool, sessions: &[AgentSessionInfo], general: &General) -> bool {
    agents_known
        && sessions.is_empty()
        && general.last_agent.is_none()
        && general.last_cwd.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-02 10:00 UTC, a Monday.
    const NOW: u64 = 1_793_613_600_000;
    const CST: i64 = 8 * 3600;

    fn session(asid: &str, status: &str, updated_ms: u64) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": asid, "title": asid, "status": status, "updated_ms": updated_ms
        }))
        .unwrap()
    }

    fn permission(id: &str, asid: &str) -> PermissionRequest {
        serde_json::from_value(serde_json::json!({
            "id": id, "asid": asid, "action": "bash", "resources": [],
            "prompt": "Run?", "options": []
        }))
        .unwrap()
    }

    #[test]
    fn local_time_follows_the_zone() {
        let t = LocalTime::at(NOW, CST);
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.weekday),
            (2026, 11, 2, 18, 1)
        );
        let utc = LocalTime::at(NOW, 0);
        assert_eq!(utc.hour, 10);
        // Ten hours west it is still Sunday the 1st.
        let west = LocalTime::at(NOW, -11 * 3600);
        assert_eq!((west.day, west.weekday, west.hour), (1, 0, 23));
        let leap = LocalTime::at(951_782_400_000, 0); // 2000-02-29
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
    }

    #[test]
    fn the_greeting_follows_the_hour() {
        assert_eq!(DayPart::of(4), DayPart::Night);
        assert_eq!(DayPart::of(5), DayPart::Morning);
        assert_eq!(DayPart::of(12), DayPart::Afternoon);
        assert_eq!(DayPart::of(18), DayPart::Evening);
        assert_eq!(DayPart::of(23), DayPart::Night);
    }

    #[test]
    fn stats_count_working_waiting_and_today() {
        let sessions = [
            session("a", "busy", NOW - 60_000),
            session("b", "retry", NOW - 2 * 86_400_000),
            session("c", "idle", NOW - 3_600_000),
        ];
        let attention = [
            permission("p1", "c"),
            permission("p2", "c"),
            permission("p3", "b"),
        ];
        let stats = Stats::of(&sessions, &attention, NOW, CST);
        assert_eq!(
            stats,
            Stats {
                working: 2,
                waiting: 2,
                today: 2
            }
        );
        assert!(Stats::of(&[], &[], NOW, CST).is_empty());
    }

    #[test]
    fn usage_is_summed_over_today_and_absent_when_unreported() {
        let mut a = session("a", "idle", NOW - 60_000);
        a.tokens = Some(
            serde_json::from_value(
                serde_json::json!({"input": 1000, "output": 500, "reasoning": 100}),
            )
            .unwrap(),
        );
        a.cost = Some(1.25);
        let mut b = session("b", "idle", NOW - 120_000);
        b.cost = Some(0.5);
        let mut old = session("old", "idle", NOW - 3 * 86_400_000);
        old.cost = Some(9.0);
        let usage = Usage::today(&[a, b, old], NOW, CST);
        assert_eq!(usage.tokens, Some(1600));
        assert_eq!(usage.cost, Some(1.75));
        assert_eq!(
            Usage::today(&[session("x", "idle", NOW)], NOW, CST),
            Usage::default()
        );
    }

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact_count(987), "987");
        assert_eq!(compact_count(12_345), "12.3k");
        assert_eq!(compact_count(128_000), "128k");
        assert_eq!(compact_count(4_200_000), "4.2M");
        assert_eq!(compact_count(10_000), "10k");
    }

    #[test]
    fn long_paths_keep_both_ends() {
        assert_eq!(middle_ellipsis("~/Work/x", 17), "~/Work/x");
        let cut = middle_ellipsis("/tmp/opencode/desktop-qa", 17);
        assert_eq!(cut, "/tmp/o…desktop-qa");
        assert_eq!(cut.chars().count(), 17);
        assert_eq!(
            middle_ellipsis("模型名字很长很长很长很长很长很长很长", 9)
                .chars()
                .count(),
            9
        );
    }

    #[test]
    fn breakpoints_follow_the_window() {
        assert_eq!(Layout::for_window(1600.0).hero_height, 240.0);
        assert!(Layout::for_window(1600.0).two_columns);
        let medium = Layout::for_window(1000.0);
        assert_eq!((medium.hero_height, medium.two_columns), (180.0, false));
        assert_eq!(Layout::for_window(900.0).hero_height, 140.0);
    }

    #[test]
    fn first_run_needs_known_agents_and_no_history() {
        let fresh = General::default();
        assert!(!first_run(false, &[], &fresh), "undecided before discovery");
        assert!(first_run(true, &[], &fresh));
        assert!(!first_run(true, &[session("a", "idle", NOW)], &fresh));
        let used = General {
            last_agent: Some("codex".into()),
            ..General::default()
        };
        assert!(!first_run(true, &[], &used));
    }
}
