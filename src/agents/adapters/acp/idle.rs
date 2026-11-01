//! Whether an idle ACP child may be released. A timer alone never decides:
//! after the idle window, every signal of work must read quiet, any one
//! that shows work re-arms the window, and one that cannot be read keeps
//! the child.
//!
//! - The agent's own session registry, where it keeps one (Claude Code's
//!   `sessions/<pid>.json`: `busy` while a turn or background subagent runs).
//!   Cursor and Antigravity keep none; for them this signal is absent.
//! - The child's process tree: any process not already there before the
//!   session's first prompt (a script, a background command, an MCP server
//!   started later) is work. Those present then (the agent itself, its
//!   configured MCP servers) are not.
//! - The session's native store, where the agent's store shows each write
//!   (Claude Code's transcript, Cursor's and Antigravity's session
//!   databases): written within the window is work.
//!
//! A generation that ran tools is released only with the store signal (and
//! the registry, where there is one); without it (FX, Qoder) it is kept.
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::agents::adapters::codex::process_tree::ProcessKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Quiet,
    Active,
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IdleVerdict {
    Retire,
    /// A signal could not be read: keep the child, look again next sweep.
    Wait,
    /// A signal shows work: the idle window starts again.
    Rearm,
}

pub(crate) struct IdleSignals {
    /// `None` for an agent that keeps no session registry.
    pub registry: Option<Work>,
    pub processes: Work,
    /// `None` for an agent whose store shows no writes.
    pub store: Option<Work>,
}

/// The process signal: `now` within `baseline`, both read.
pub(crate) fn process_work(
    now: Option<&HashSet<ProcessKey>>,
    baseline: Option<&HashSet<ProcessKey>>,
) -> Work {
    match (now, baseline) {
        (Some(now), Some(baseline)) if now.is_subset(baseline) => Work::Quiet,
        (Some(_), Some(_)) => Work::Active,
        _ => Work::Unknown,
    }
}

/// The store signal: last written `modified`, quiet for the whole window.
pub(crate) fn store_work(modified: Option<SystemTime>, now: SystemTime, window: Duration) -> Work {
    match modified.map(|modified| now.duration_since(modified)) {
        Some(Ok(quiet)) if quiet >= window => Work::Quiet,
        // A store written "in the future" is not quiet either.
        Some(_) => Work::Active,
        None => Work::Unknown,
    }
}

/// When an SQLite store was last written: its file or the journal beside
/// it, where writes land before a checkpoint. `None` without the store.
pub(crate) fn sqlite_modified(db: &std::path::Path) -> Option<SystemTime> {
    if !std::fs::symlink_metadata(db).ok()?.is_file() {
        return None;
    }
    let mut latest = std::fs::metadata(db).ok()?.modified().ok()?;
    for suffix in ["-wal", "-journal"] {
        let mut side = db.as_os_str().to_owned();
        side.push(suffix);
        match std::fs::metadata(&side) {
            Ok(stat) => latest = latest.max(stat.modified().ok()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(latest)
}

pub(crate) fn idle_verdict(signals: &IdleSignals) -> IdleVerdict {
    let signals = [
        signals.registry.unwrap_or(Work::Quiet),
        signals.processes,
        signals.store.unwrap_or(Work::Quiet),
    ];
    if signals.contains(&Work::Active) {
        IdleVerdict::Rearm
    } else if signals.contains(&Work::Unknown) {
        IdleVerdict::Wait
    } else {
        IdleVerdict::Retire
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_is_released_only_when_every_signal_reads_quiet() {
        use IdleVerdict::{Rearm, Retire, Wait};
        let window = Duration::from_secs(300);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let old = Some(now - Duration::from_secs(301));
        let recent = Some(now - Duration::from_secs(10));
        let agent = HashSet::from([(10, (1, 0)), (11, (2, 0))]);
        let script = HashSet::from([(10, (1, 0)), (11, (2, 0)), (12, (9, 0))]);
        // The same pid, started again later, is another process.
        let reused = HashSet::from([(10, (1, 0)), (11, (7, 0))]);
        let gone = HashSet::from([(10, (1, 0))]);
        for (name, registry, now_tree, baseline, store, want) in [
            (
                "claude, all quiet",
                Some(Work::Quiet),
                Some(&agent),
                Some(&agent),
                old,
                Retire,
            ),
            (
                "agent without registry",
                None,
                Some(&agent),
                Some(&agent),
                old,
                Retire,
            ),
            (
                "a baseline process exited",
                None,
                Some(&gone),
                Some(&agent),
                old,
                Retire,
            ),
            (
                "registry busy (subagent)",
                Some(Work::Active),
                Some(&agent),
                Some(&agent),
                old,
                Rearm,
            ),
            (
                "registry entry missing",
                Some(Work::Unknown),
                Some(&agent),
                Some(&agent),
                old,
                Wait,
            ),
            (
                "a script still runs",
                Some(Work::Quiet),
                Some(&script),
                Some(&agent),
                old,
                Rearm,
            ),
            (
                "a pid reused",
                None,
                Some(&reused),
                Some(&agent),
                old,
                Rearm,
            ),
            (
                "process tree unreadable",
                Some(Work::Quiet),
                None,
                Some(&agent),
                old,
                Wait,
            ),
            ("no baseline", None, Some(&agent), None, old, Wait),
            (
                "store written in the window",
                Some(Work::Quiet),
                Some(&agent),
                Some(&agent),
                recent,
                Rearm,
            ),
            (
                "store unreadable",
                Some(Work::Quiet),
                Some(&agent),
                Some(&agent),
                None,
                Wait,
            ),
            (
                "work beats unknown",
                Some(Work::Unknown),
                Some(&script),
                Some(&agent),
                None,
                Rearm,
            ),
            (
                "store written later than now",
                None,
                Some(&agent),
                Some(&agent),
                Some(now + Duration::from_secs(1)),
                Rearm,
            ),
        ] {
            let signals = IdleSignals {
                registry,
                processes: process_work(now_tree, baseline),
                store: Some(store_work(store, now, window)),
            };
            assert_eq!(idle_verdict(&signals), want, "{name}");
        }
    }
}
